//! The command palette: a box at the top of the main window that finds and runs
//! anything by typing a few letters. Commands, open tabs, connections, the tables of open
//! connections and the pages of Settings come from [`providers`]; `>` keeps to commands,
//! `@` to objects and `#` to connections. Items used recently rank first.
//!
//! The items are gathered once when the palette opens and filtered on every keystroke
//! with a [`fuzzy::Pattern`] folded once per keystroke. Only the best [`MAX_HITS`] are
//! ordered and listed, and only the visible rows are drawn, so a database with tens of
//! thousands of tables stays quick to filter.

pub mod fuzzy;
pub mod providers;
pub mod recent;

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    HighlightStyle, IntoElement, KeyBinding, Render, ScrollStrategy, StyledText, Subscription,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

use crate::state::AppState;
use crate::theme::theme;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::workspace::Workspace;
use fuzzy::{Pattern, fuzzy_match};
use providers::{PaletteItem, Scope, Sources, parse_query};
use recent::Recent;

actions!(palette, [SelectNext, SelectPrevious, Complete]);

/// The palette's own keys. Enter and Escape come from the text field.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some("CommandPalette");
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        // Ahead of the text field's own Tab, which moves the focus.
        KeyBinding::new("tab", Complete, Some("CommandPalette > TextInput")),
    ]);
}

/// Rows shown at once before the list scrolls.
const VISIBLE_ROWS: f32 = 10.;
/// The most a recently used item gains, for the very latest one.
const RECENT_BONUS: i32 = 36;
/// What a match in an item's extra search text gives up against a title match.
const SEARCH_PENALTY: i32 = 24;
/// The most items listed for one query.
pub const MAX_HITS: usize = 500;

/// An item that matched, with its score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub item: usize,
    pub score: i32,
}

/// Matches and orders `items` for `query`, keeping the best [`MAX_HITS`]. With nothing
/// typed, recently used items come first, newest first, and the rest keep the
/// providers' order.
pub fn rank(items: &[PaletteItem], query: &str, recent: &Recent) -> Vec<Hit> {
    let (scope, text) = parse_query(query);
    let pattern = Pattern::new(text);
    let typed = pattern.len() as i32;
    let recency: HashMap<&str, usize> = recent
        .ids
        .iter()
        .enumerate()
        .map(|(rank, id)| (id.as_str(), rank))
        .collect();
    // Each hit with its place in the order: recency with nothing typed, else score, then
    // shorter titles, then the providers' order.
    let mut hits: Vec<(Hit, (i64, usize, usize))> = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        if !scope.admits(item.kind) {
            continue;
        }
        let score = if pattern.is_empty() {
            0
        } else {
            match pattern.matches(&item.title) {
                Some(m) => m.score,
                // The extra text is long and would match nearly anything loosely, so
                // only a tight match there counts.
                None => match item.search.as_deref().and_then(|s| pattern.matches(s)) {
                    Some(m) if m.score >= fuzzy::MATCH * typed => m.score - SEARCH_PENALTY,
                    _ => continue,
                },
            }
        };
        let recency = recency.get(item.id.as_str()).copied();
        let bonus = recency.map_or(0, |r| (RECENT_BONUS - 3 * r as i32).max(0));
        let score = score + bonus;
        let key = if pattern.is_empty() {
            (recency.map_or(i64::MAX, |r| r as i64), 0, ix)
        } else {
            (-i64::from(score), item.title.len(), ix)
        };
        hits.push((Hit { item: ix, score }, key));
    }
    // Only the first rows are ever looked at; ordering tens of thousands of tables
    // on every keystroke would be wasted work.
    if hits.len() > MAX_HITS {
        hits.select_nth_unstable_by_key(MAX_HITS, |(_, key)| *key);
        hits.truncate(MAX_HITS);
    }
    hits.sort_unstable_by_key(|(_, key)| *key);
    hits.into_iter().map(|(h, _)| h).collect()
}

pub enum PaletteEvent {
    /// The palette closed, after running an item or not.
    Dismissed,
}

pub struct CommandPalette {
    items: Rc<Vec<PaletteItem>>,
    hits: Rc<Vec<Hit>>,
    selected: usize,
    input: Entity<TextInput>,
    scroll: UniformListScrollHandle,
    /// Where the focus goes back to when the palette closes.
    restore_focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    /// A palette over everything the providers list now, with `query` typed.
    pub fn new(
        state: &Entity<AppState>,
        workspace: &Entity<Workspace>,
        query: &str,
        restore_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let items = providers::collect(&Sources {
            state,
            workspace,
            cx,
        });
        let input = cx.new(|cx| {
            let mut i = TextInput::new("Search commands, tables, connections…", cx);
            i.set_text(query.to_owned(), cx);
            i
        });
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, e, window, cx| match e {
                InputEvent::Changed => this.refilter(cx),
                InputEvent::Submit => this.confirm(window, cx),
                InputEvent::Cancel => this.dismiss(window, cx),
            }),
        ];
        let mut this = Self {
            items: Rc::new(items),
            hits: Rc::default(),
            selected: 0,
            input,
            scroll: UniformListScrollHandle::new(),
            restore_focus,
            _subs: subs,
        };
        this.refilter(cx);
        this
    }

    /// Replaces the query, as when Ctrl+P is pressed with the palette already open.
    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.input
            .update(cx, |i, cx| i.set_text(query.to_owned(), cx));
    }

    pub fn query(&self, cx: &App) -> String {
        self.input.read(cx).text().to_owned()
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let recent = cx.try_global::<Recent>().cloned().unwrap_or_default();
        self.hits = Rc::new(rank(&self.items, &query, &recent));
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn select(&mut self, step: isize, cx: &mut Context<Self>) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + step).rem_euclid(n as isize) as usize;
        let strategy = if step > 0 {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        };
        self.scroll.scroll_to_item(self.selected, strategy);
        cx.notify();
    }

    /// Tab: the selected item's title becomes the query, keeping the scope prefix.
    fn complete(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(self.selected) else {
            return;
        };
        let (scope, _) = parse_query(&self.query(cx));
        let text = format!("{}{}", scope.prefix(), self.items[hit.item].title);
        self.input.update(cx, |i, cx| i.set_text(text, cx));
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self
            .hits
            .get(self.selected)
            .map(|h| self.items[h.item].clone())
        else {
            return;
        };
        recent::record(&item.id, cx);
        self.dismiss(window, cx);
        (item.run)(window, cx);
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.restore_focus);
        cx.emit(PaletteEvent::Dismissed);
    }

    fn row(&self, position: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = theme(cx).clone();
        let Some(hit) = self.hits.get(position) else {
            return div().into_any_element();
        };
        let item = &self.items[hit.item];
        let selected = position == self.selected;
        let query = self.query(cx);
        let (_, text) = parse_query(&query);
        let highlight = HighlightStyle {
            color: Some(t.accent),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let ranges = fuzzy_match(text, &item.title)
            .map(|m| m.ranges(&item.title))
            .unwrap_or_default();
        let title = StyledText::new(item.title.clone())
            .with_highlights(ranges.into_iter().map(|r| (r, highlight)));
        let right: gpui::AnyElement = match &item.shortcut {
            Some(keys) => div()
                .flex_none()
                .h(t.s(18.))
                .px(px(5.))
                .flex()
                .items_center()
                .rounded(t.radius_small)
                .border_1()
                .border_color(t.border)
                .text_size(t.small_size)
                .text_color(t.text_muted)
                .child(keys.clone())
                .into_any_element(),
            None => div()
                .flex_none()
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(item.kind.label())
                .into_any_element(),
        };
        div()
            .id(position)
            .w_full()
            .h(t.s(30.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(t.radius_small)
            .cursor_pointer()
            .when(selected, |el| el.bg(t.selected))
            .when(!selected, |el| el.hover(|s| s.bg(t.hover)))
            .child(icon(item.icon).size(px(13.)).text_color(if selected {
                t.accent
            } else {
                t.text_faint
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .max_w(gpui::relative(0.7))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(t.text)
                            .child(title),
                    )
                    .children(item.detail.clone().map(|d| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(d)
                    })),
            )
            .child(right)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = position;
                this.confirm(window, cx);
            }))
            .into_any_element()
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let count = self.hits.len();
        let row_height = t.s(30.);
        let list_height = row_height * (count as f32).min(VISIBLE_ROWS) + px(8.);
        let focused = self.input.focus_handle(cx).is_focused(window);
        let list = if count == 0 {
            div()
                .h(t.s(64.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child("Nothing matches")
                .into_any_element()
        } else {
            uniform_list(
                "palette-items",
                count,
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range.map(|i| this.row(i, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(self.scroll.clone())
            .h(list_height)
            .px(px(4.))
            .py(px(4.))
            .into_any_element()
        };
        let hint = |key: &'static str, what: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(div().text_color(t.text_muted).child(key))
                .child(what)
        };
        div()
            .id("command-palette")
            .key_context("CommandPalette")
            .occlude()
            .w(px(580.))
            .max_w(gpui::relative(0.92))
            .flex()
            .flex_col()
            .rounded(t.radius)
            .bg(t.floating)
            .border_1()
            .border_color(t.border_strong)
            .shadow_lg()
            .overflow_hidden()
            .font_family(t.ui_font.clone())
            .text_size(t.ui_size)
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.select(-1, cx)))
            .on_action(cx.listener(|this, _: &Complete, _, cx| this.complete(cx)))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.dismiss(window, cx)))
            .child(
                div()
                    .h(t.s(40.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(if focused { t.border_strong } else { t.border })
                    .child(icon(Icon::Search).size(px(14.)).text_color(t.text_faint))
                    .child(div().flex_1().min_w_0().child(self.input.clone())),
            )
            .child(list)
            .child(
                div()
                    .h(t.s(26.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .border_t_1()
                    .border_color(t.border)
                    .bg(t.surface_alt)
                    .text_size(t.small_size)
                    .text_color(t.text_faint)
                    .child(hint(">", "commands"))
                    .child(hint("@", "objects"))
                    .child(hint("#", "connections"))
                    .child(div().flex_1())
                    .child(hint("Tab", "complete"))
                    .child(hint("Enter", "run")),
            )
    }
}

/// The palette's place in the main window: nothing while it is closed, the palette over
/// the top of the window while it is open.
pub struct PaletteLayer {
    state: Entity<AppState>,
    workspace: Entity<Workspace>,
    /// Where the focus goes when nothing had it before the palette opened.
    fallback_focus: FocusHandle,
    open: Option<(Entity<CommandPalette>, Subscription)>,
}

impl PaletteLayer {
    pub fn new(
        state: Entity<AppState>,
        workspace: Entity<Workspace>,
        fallback_focus: FocusHandle,
    ) -> Self {
        Self {
            state,
            workspace,
            fallback_focus,
            open: None,
        }
    }

    /// Opens the palette in `scope`, or switches the open one to it. Pressed again with
    /// nothing else typed, the shortcut closes it.
    pub fn toggle(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let query = scope.prefix();
        if let Some((p, _)) = &self.open {
            let p = p.clone();
            if p.read(cx).query(cx) == query {
                p.update(cx, |p, cx| p.dismiss(window, cx));
            } else {
                p.update(cx, |p, cx| p.set_query(query, cx));
                window.focus(&p.focus_handle(cx));
            }
            return;
        }
        let restore = window
            .focused(cx)
            .unwrap_or_else(|| self.fallback_focus.clone());
        let (state, workspace) = (self.state.clone(), self.workspace.clone());
        let palette =
            cx.new(|cx| CommandPalette::new(&state, &workspace, query, restore, window, cx));
        window.focus(&palette.focus_handle(cx));
        let sub = cx.subscribe(&palette, |this, _, e, cx| match e {
            PaletteEvent::Dismissed => {
                this.open = None;
                cx.notify();
            }
        });
        self.open = Some((palette, sub));
        cx.notify();
    }
}

impl Render for PaletteLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((palette, _)) = &self.open else {
            return div().into_any_element();
        };
        let t = theme(cx);
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(t.s(48.))
            .child(gpui::deferred(palette.clone()).with_priority(3))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::Kind;

    fn item(id: &str, kind: Kind, title: &str) -> PaletteItem {
        PaletteItem::new(id, kind, Icon::Table, title.to_owned(), |_, _| {})
    }

    fn titles(items: &[PaletteItem], hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|h| items[h.item].title.to_string())
            .collect()
    }

    fn recent(ids: &[&str]) -> Recent {
        let mut r = Recent::default();
        r.ids = ids.iter().map(|s| (*s).to_owned()).collect();
        r
    }

    #[test]
    fn nothing_typed_lists_recent_first() {
        let items = vec![
            item("a", Kind::Action, "New query tab"),
            item("b", Kind::Action, "Close tab"),
            item("c", Kind::Object, "orders"),
            item("d", Kind::Object, "users"),
        ];
        let hits = rank(&items, "", &recent(&["d", "b"]));
        assert_eq!(
            titles(&items, &hits),
            ["users", "Close tab", "New query tab", "orders"]
        );
    }

    #[test]
    fn best_match_first_and_recent_breaks_close_calls() {
        let items = vec![
            item("a", Kind::Object, "order_items"),
            item("b", Kind::Object, "orders"),
            item("c", Kind::Object, "users"),
        ];
        let hits = rank(&items, "orders", &Recent::default());
        assert_eq!(titles(&items, &hits), ["orders", "order_items"]);
        let hits = rank(&items, "users", &Recent::default());
        assert_eq!(titles(&items, &hits), ["users"]);
        let hits = rank(&items, "ord", &Recent::default());
        assert_eq!(titles(&items, &hits), ["orders", "order_items"]);
        let hits = rank(&items, "ord", &recent(&["a"]));
        assert_eq!(titles(&items, &hits), ["order_items", "orders"]);
    }

    #[test]
    fn prefixes_filter_kinds() {
        let items = vec![
            item("a", Kind::Action, "Reload data"),
            item("b", Kind::Object, "readings"),
            item("c", Kind::Connection, "Reporting"),
        ];
        let none = Recent::default();
        assert_eq!(titles(&items, &rank(&items, ">re", &none)), ["Reload data"]);
        assert_eq!(titles(&items, &rank(&items, "@re", &none)), ["readings"]);
        assert_eq!(titles(&items, &rank(&items, "#", &none)), ["Reporting"]);
        assert_eq!(rank(&items, "re", &none).len(), 3);
    }

    #[test]
    fn many_matches_are_cut_to_the_best() {
        let mut items: Vec<PaletteItem> = (0..20_000)
            .map(|n| item(&format!("t{n}"), Kind::Object, &format!("table_{n:05}")))
            .collect();
        items.push(item("best", Kind::Object, "tab"));
        let hits = rank(&items, "tab", &Recent::default());
        assert_eq!(hits.len(), MAX_HITS);
        assert_eq!(items[hits[0].item].title.as_ref(), "tab");
        // Equal scores keep the providers' order.
        assert_eq!(items[hits[1].item].title.as_ref(), "table_00000");
        let all = rank(&items, "", &recent(&["t19999"]));
        assert_eq!(all.len(), MAX_HITS);
        assert_eq!(items[all[0].item].title.as_ref(), "table_19999");
        assert_eq!(items[all[1].item].title.as_ref(), "table_00000");
    }

    #[test]
    fn extra_search_text_finds_items_below_title_matches() {
        let items = vec![
            item("a", Kind::Object, "orders").search("sales.orders"),
            item("b", Kind::Object, "sales_targets"),
        ];
        let hits = rank(&items, "sales", &Recent::default());
        assert_eq!(titles(&items, &hits), ["sales_targets", "orders"]);
        // Letters strewn across the extra text do not count.
        let items = vec![item("a", Kind::Object, "documents").search("demo.archive.documents")];
        assert!(rank(&items, "ord", &Recent::default()).is_empty());
    }
}
