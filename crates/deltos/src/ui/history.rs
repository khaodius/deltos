//! Statements that ran, newest first, and the saved queries. Both lists can be searched
//! and narrowed to the active connection. Clicking a statement or a saved query opens it
//! in a new query tab; nothing runs until asked.

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px, uniform_list,
};

use crate::history_file;
use crate::snippets::{self, Snippet};
use crate::state::{AppState, HistoryEntry, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::icons::Icon;
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{
    ButtonStyle, MenuItem, Tooltip, button, empty_state, field, icon_button, menu, segmented,
    thousands,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    History,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The active connection's, plus saved queries for every connection.
    This,
    All,
}

struct Rename {
    id: String,
    input: Entity<TextInput>,
    error: Option<String>,
    _sub: Subscription,
}

pub struct HistoryPanel {
    state: Entity<AppState>,
    search: Entity<TextInput>,
    tab: Tab,
    scope: Scope,
    /// The history entries shown, as positions in the state's history, newest first.
    rows: Rc<Vec<usize>>,
    /// Asking whether to clear the history.
    clearing: bool,
    /// The saved query whose menu is open.
    menu: Option<String>,
    renaming: Option<Rename>,
    _subs: Vec<Subscription>,
}

impl HistoryPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search", cx));
        let mut subs = vec![
            cx.subscribe(&state, |this, _, e, cx| match e {
                StateEvent::History | StateEvent::ActiveSession => {
                    this.refilter(cx);
                    cx.notify();
                }
                StateEvent::Settings | StateEvent::Sessions => cx.notify(),
                _ => {}
            }),
            cx.subscribe(&search, |this, input, e, cx| match e {
                InputEvent::Changed => {
                    this.refilter(cx);
                    cx.notify();
                }
                InputEvent::Cancel => input.update(cx, |i, cx| i.clear(cx)),
                InputEvent::Submit => {}
            }),
        ];
        if let Some(store) = snippets::store(cx) {
            subs.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            state,
            search,
            tab: Tab::History,
            scope: Scope::This,
            rows: Rc::new(Vec::new()),
            clearing: false,
            menu: None,
            renaming: None,
            _subs: subs,
        };
        this.refilter(cx);
        this
    }

    /// The connection the list is narrowed to, if it is.
    fn connection(&self, cx: &App) -> Option<String> {
        match self.scope {
            Scope::This => self.state.read(cx).active.clone(),
            Scope::All => None,
        }
    }

    fn needle(&self, cx: &App) -> String {
        self.search.read(cx).text().trim().to_lowercase()
    }

    fn refilter(&mut self, cx: &App) {
        let needle = self.needle(cx);
        let connection = self.connection(cx);
        let history = &self.state.read(cx).history;
        let rows = history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| connection.as_deref().is_none_or(|c| e.connection == c))
            .filter(|(_, e)| contains(&e.statement, &needle))
            .map(|(i, _)| i)
            .collect();
        self.rows = Rc::new(rows);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.clearing = false;
        self.refilter(cx);
        cx.notify();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.clearing = false;
        self.menu = None;
        self.renaming = None;
        cx.notify();
    }

    fn connection_name(&self, id: &str, cx: &App) -> String {
        let s = self.state.read(cx);
        s.session(id)
            .map(|x| x.name.clone())
            .or_else(|| s.settings.connection(id).map(|c| c.name.clone()))
            .unwrap_or_else(|| id.to_owned())
    }

    fn start_rename(&mut self, snippet: &Snippet, window: &mut Window, cx: &mut Context<Self>) {
        let id = snippet.id.clone();
        let input = cx.new(|cx| {
            let mut i = TextInput::new("Name", cx);
            i.set_text(snippet.name.clone(), cx);
            i.select_everything(cx);
            i
        });
        window.focus(&input.focus_handle(cx));
        let sub = cx.subscribe(&input, |this, input, e, cx| match e {
            InputEvent::Submit => {
                let Some(id) = this.renaming.as_ref().map(|r| r.id.clone()) else {
                    return;
                };
                let name = input.read(cx).text().to_owned();
                let result = snippets::store(cx)
                    .map_or(Ok(()), |s| s.update(cx, |s, cx| s.rename(&id, &name, cx)));
                match result {
                    Ok(()) => this.renaming = None,
                    Err(e) => {
                        if let Some(r) = this.renaming.as_mut() {
                            r.error = Some(e);
                        }
                    }
                }
                cx.notify();
            }
            InputEvent::Cancel => {
                this.renaming = None;
                cx.notify();
            }
            InputEvent::Changed => {
                if let Some(r) = this.renaming.as_mut() {
                    r.error = None;
                }
                cx.notify();
            }
        });
        self.renaming = Some(Rename {
            id,
            input,
            error: None,
            _sub: sub,
        });
        self.menu = None;
        cx.notify();
    }

    fn header(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let w2 = weak.clone();
        let count = match self.tab {
            Tab::History => self.rows.len(),
            Tab::Saved => self.saved(cx).len(),
        };
        let can_clear = self.tab == Tab::History && !self.rows.is_empty();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(segmented(
                        "history-tab",
                        &[("History", Tab::History), ("Saved", Tab::Saved)],
                        self.tab,
                        move |tab, _, cx| {
                            weak.update(cx, |this, cx| this.set_tab(tab, cx)).ok();
                        },
                        t,
                    ))
                    .child(segmented(
                        "history-scope",
                        &[("This connection", Scope::This), ("All", Scope::All)],
                        self.scope,
                        move |scope, _, cx| {
                            w2.update(cx, |this, cx| this.set_scope(scope, cx)).ok();
                        },
                        t,
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(thousands(count as u64)),
                    )
                    .when(can_clear && !self.clearing, |el| {
                        el.child(
                            button("history-clear", "Clear", ButtonStyle::Ghost, t)
                                .h(t.s(22.))
                                .px(px(8.))
                                .text_size(t.small_size)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clearing = true;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(field(&self.search, t, window, cx))
            .when(self.clearing, |el| {
                let what = match self.connection(cx) {
                    Some(id) => format!("Clear the history of {}?", self.connection_name(&id, cx)),
                    None => "Clear the whole history?".to_owned(),
                };
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(t.small_size)
                        .child(div().flex_1().text_color(t.text).child(what))
                        .child(
                            button("history-clear-no", "Cancel", ButtonStyle::Ghost, t)
                                .h(t.s(22.))
                                .px(px(8.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clearing = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            button("history-clear-yes", "Clear", ButtonStyle::Danger, t)
                                .h(t.s(22.))
                                .px(px(8.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clearing = false;
                                    let connection = this.connection(cx);
                                    history_file::clear(&this.state, connection.as_deref(), cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    fn history_list(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            let (title, detail) = if self.state.read(cx).history.is_empty() {
                ("No statements yet", "Queries you run appear here.")
            } else {
                ("Nothing matches", "Try another search, or all connections.")
            };
            return empty_state(title, Some(detail.into()), t).into_any_element();
        }
        let rows = self.rows.clone();
        let state = self.state.clone();
        let panel = cx.entity().downgrade();
        let t = t.clone();
        let now = SystemTime::now();
        let show_connection = self.connection(cx).is_none();
        uniform_list("history", rows.len(), move |range, _, cx| {
            let s = state.read(cx);
            range
                .filter_map(|i| {
                    let e = s.history.get(rows[i])?;
                    let conn = show_connection.then(|| {
                        panel
                            .upgrade()
                            .map(|p| p.read(cx).connection_name(&e.connection, cx))
                            .unwrap_or_else(|| e.connection.clone())
                    });
                    Some(history_row(i, e, conn, now, &state, &t))
                })
                .collect()
        })
        .flex_1()
        .into_any_element()
    }

    /// The saved queries shown: those of the active connection or all, that match.
    fn saved(&self, cx: &App) -> Vec<Snippet> {
        let needle = self.needle(cx);
        let list = match self.connection(cx) {
            Some(c) => snippets::snippets_for(Some(&c), cx),
            None => snippets::all_snippets(cx),
        };
        list.into_iter()
            .filter(|s| contains(&s.name, &needle) || contains(&s.text, &needle))
            .collect()
    }

    fn saved_list(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let items = self.saved(cx);
        if items.is_empty() {
            let none = snippets::store(cx).is_none_or(|s| s.read(cx).items().is_empty());
            let (title, detail): (&str, SharedString) = if none {
                let keys = self.state.read(cx).settings.keybinds.get("save-query");
                (
                    "No saved queries",
                    format!(
                        "Save a query with {} in its editor.",
                        crate::ui::query::key_label(keys)
                    )
                    .into(),
                )
            } else {
                (
                    "Nothing matches",
                    "Try another search, or all connections.".into(),
                )
            };
            return empty_state(title, Some(detail), t).into_any_element();
        }
        let mut list = div()
            .id("saved-queries")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (ix, snippet) in items.into_iter().enumerate() {
            let row = self.saved_row(ix, snippet, t, window, cx);
            list = list.child(row);
        }
        list.into_any_element()
    }

    fn saved_row(
        &mut self,
        ix: usize,
        snippet: Snippet,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = match &snippet.connection {
            Some(id) => self.connection_name(id, cx),
            None => "All connections".to_owned(),
        };
        let renaming = self
            .renaming
            .as_ref()
            .filter(|r| r.id == snippet.id)
            .map(|r| (r.input.clone(), r.error.clone()));
        let menu_open = self.menu.as_deref() == Some(snippet.id.as_str());
        let state = self.state.clone();
        let snip = Rc::new(snippet);
        let name: AnyElement = match renaming {
            Some((input, error)) => div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(field(&input, t, window, cx))
                .children(
                    error.map(|e| div().text_size(t.small_size).text_color(t.danger).child(e)),
                )
                .into_any_element(),
            None => div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(t.ui_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text)
                .child(snip.name.clone())
                .into_any_element(),
        };
        let more = {
            let id = snip.id.clone();
            let mut b = icon_button(("saved-more", ix), Icon::More, t)
                .when(!menu_open, |el| el.invisible())
                .group_hover("saved-row", |s| s.visible())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.menu = if this.menu.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id.clone())
                    };
                    cx.notify();
                }))
                .into_any_element();
            if menu_open {
                b = div()
                    .child(b)
                    .child(self.saved_menu(ix, &snip, t, cx))
                    .into_any_element();
            }
            b
        };
        let s2 = snip.clone();
        let s3 = snip.clone();
        let state3 = self.state.clone();
        div()
            .id(("saved", ix))
            .group("saved-row")
            .flex_none()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(t.grid_line)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .on_click(move |_, _, cx| snippets::open(&state, &s2, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(name)
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(t.mono_font.clone())
                            .text_size(t.small_size)
                            .text_color(t.text_muted)
                            .child(one_line(&snip.text)),
                    )
                    .child(
                        div()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(scope),
                    ),
            )
            .child(
                icon_button(("saved-insert", ix), Icon::Plus, t)
                    .invisible()
                    .group_hover("saved-row", |s| s.visible())
                    .tooltip(|_, cx| Tooltip::view("Insert at the cursor of the last query", cx))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        insert_or_open(&state3, &s3, cx);
                    }),
            )
            .child(more)
            .into_any_element()
    }

    fn saved_menu(
        &mut self,
        ix: usize,
        snippet: &Rc<Snippet>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let state = self.state.clone();
        let state2 = self.state.clone();
        let (s1, s2, s3, s4) = (
            snippet.clone(),
            snippet.clone(),
            snippet.clone(),
            snippet.clone(),
        );
        let w_rename = weak.clone();
        let w_close = weak.clone();
        let items = vec![
            MenuItem::new("Open in a new tab", move |_, cx| {
                snippets::open(&state, &s1, cx)
            }),
            MenuItem::new("Insert at cursor", move |_, cx| {
                insert_or_open(&state2, &s2, cx)
            }),
            MenuItem::new("Copy text", move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(s3.text.clone()))
            }),
            MenuItem::new("Rename", move |window, cx| {
                let s = s4.clone();
                w_rename
                    .update(cx, |this, cx| this.start_rename(&s, window, cx))
                    .ok();
            }),
            MenuItem::separator(),
            {
                let id = snippet.id.clone();
                MenuItem::new("Delete", move |_, cx| {
                    if let Some(store) = snippets::store(cx) {
                        store.update(cx, |s, cx| s.delete(&id, cx));
                    }
                })
                .danger()
            },
        ];
        menu(
            ("saved-menu", ix),
            items,
            180.,
            move |_, cx| {
                w_close
                    .update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    })
                    .ok();
            },
            t,
        )
    }
}

/// Types the snippet into the query editor used last, or opens it when there is none.
fn insert_or_open(state: &Entity<AppState>, snippet: &Snippet, cx: &mut App) {
    if !snippets::insert_at_cursor(&snippet.text, cx) {
        snippets::open(state, snippet, cx);
    }
}

fn history_row(
    i: usize,
    e: &HistoryEntry,
    connection: Option<String>,
    now: SystemTime,
    state: &Entity<AppState>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let mut facts = Vec::with_capacity(4);
    if let Some(c) = connection {
        facts.push(c);
    }
    facts.push(age(e.at, now));
    facts.push(duration(e.duration));
    let (detail, failed) = match (&e.error, e.rows) {
        (Some(err), _) => (one_line(err), true),
        (None, Some(n)) => (
            format!("{} {}", thousands(n), if n == 1 { "row" } else { "rows" }),
            false,
        ),
        (None, None) => (String::new(), false),
    };
    let open = {
        let state = state.clone();
        let kind = deltos_settings::PanelKind::Query {
            connection: Some(e.connection.clone()),
            text: e.statement.clone(),
        };
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            let kind = kind.clone();
            state.update(cx, |s, cx| s.open(kind, true, cx));
        }
    };
    let copy_text = e.statement.clone();
    let (at, statement) = (e.at, e.statement.clone());
    let state_del = state.clone();
    div()
        .id(i)
        .group("history-row")
        .w_full()
        .h(t.list_row * 2.)
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(6.))
        .border_b_1()
        .border_color(t.grid_line)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .on_click(open)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(t.mono_font.clone())
                        .text_size(t.mono_size)
                        .text_color(t.text)
                        .child(one_line(&e.statement)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(facts.join("  ·  "))
                        .when(!detail.is_empty(), |el| {
                            el.child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(if failed { t.danger } else { t.text_muted })
                                    .child(detail),
                            )
                        }),
                ),
        )
        .child(
            icon_button(("history-copy", i), Icon::Copy, t)
                .invisible()
                .group_hover("history-row", |s| s.visible())
                .tooltip(|_, cx| Tooltip::view("Copy the statement", cx))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                }),
        )
        .child(
            icon_button(("history-delete", i), Icon::Trash, t)
                .invisible()
                .group_hover("history-row", |s| s.visible())
                .tooltip(|_, cx| Tooltip::view("Remove from the history", cx))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    history_file::delete(&state_del, at, &statement, cx);
                }),
        )
}

/// Whether `hay` holds `needle`, which is lowercase, in any case.
fn contains(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        return h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().contains(needle)
}

/// The text on one line, cut to a length a row can show.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(240));
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.len() >= 240 {
            let mut cut = 240;
            while !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
            out.push('…');
            break;
        }
    }
    out
}

fn duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 10_000.0 {
        format!("{:.0} s", ms / 1000.0)
    } else if ms >= 1000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        format!("{ms:.0} ms")
    }
}

/// How long ago `at` was: minutes, hours and days, then the date (UTC).
fn age(at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(at).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..2_592_000 => format!("{} d ago", secs / 86_400),
        _ => {
            let days = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 86_400;
            let (y, m, d) = civil(days as i64);
            format!("{y:04}-{m:02}-{d:02}")
        }
    }
}

/// The calendar date of a day number since 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

impl Render for HistoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let header = self.header(&t, window, cx);
        let list = match self.tab {
            Tab::History => self.history_list(&t, cx),
            Tab::Saved => self.saved_list(&t, window, cx),
        };
        div()
            .size_full()
            .bg(t.sidebar)
            .flex()
            .flex_col()
            .child(header)
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn search_ignores_case() {
        assert!(contains("SELECT * FROM Orders", "orders"));
        assert!(contains("anything", ""));
        assert!(!contains("abc", "abcd"));
        assert!(contains("Größe", "größe"));
        assert!(!contains("select", "delete"));
    }

    #[test]
    fn ages_read_naturally() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let ago = |s: u64| age(now - Duration::from_secs(s), now);
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(125), "2 min ago");
        assert_eq!(ago(7_300), "2 h ago");
        assert_eq!(ago(3 * 86_400), "3 d ago");
        assert_eq!(ago(40 * 86_400), "2023-10-05");
        assert_eq!(age(now + Duration::from_secs(9), now), "just now");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
    }

    #[test]
    fn rows_show_one_short_line() {
        assert_eq!(
            one_line("select *\n  from t\twhere x"),
            "select * from t where x"
        );
        let long = one_line(&"é ".repeat(500));
        assert!(long.len() <= 240 + '…'.len_utf8());
        assert!(long.ends_with('…'));
        assert_eq!(duration(Duration::from_millis(12)), "12 ms");
        assert_eq!(duration(Duration::from_millis(2_500)), "2.5 s");
        assert_eq!(duration(Duration::from_secs(75)), "75 s");
    }
}
