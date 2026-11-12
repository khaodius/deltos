//! Views, and the small components they share.

pub mod color_picker;
pub mod completion;
pub mod connections;
pub mod dock;
pub mod edit;
pub mod editor;
pub mod environment;
pub mod grid;
pub mod history;
pub mod icons;
pub mod inspector;
pub mod motion;
pub mod palette;
pub mod panel_window;
pub mod pending;
pub mod plan_view;
pub mod query;
pub mod save_query;
pub mod screen_color;
pub mod settings;
pub mod sidebar;
pub mod table;
pub mod text_input;
pub mod title_bar;
pub mod tsv;
pub mod workspace;

use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, Focusable, FontWeight, Hsla, IntoElement,
    Render, SharedString, Stateful, Window, div, prelude::*, px,
};

use crate::theme::{Theme, theme};
use icons::{Icon, icon};
use text_input::TextInput;

/// A small floating label shown on hover.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    pub fn view(text: impl Into<SharedString>, cx: &mut App) -> gpui::AnyView {
        cx.new(|_| Self { text: text.into() }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(t.radius_small)
            .bg(t.floating)
            .border_1()
            .border_color(t.border)
            .shadow_md()
            .font_family(t.ui_font.clone())
            .text_size(t.small_size)
            .text_color(t.text)
            .child(self.text.clone())
    }
}

/// A square button with one icon, as in toolbars.
pub fn icon_button(id: impl Into<ElementId>, which: Icon, t: &Theme) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .size(t.s(22.))
        .rounded(t.radius_small)
        .flex()
        .items_center()
        .justify_center()
        .text_color(t.text_muted)
        .cursor_pointer()
        .hover(|s| s.bg(t.hover).text_color(t.text))
        .child(icon(which).size(t.s(14.)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    style: ButtonStyle,
    t: &Theme,
) -> Stateful<Div> {
    let base = div()
        .id(id.into())
        .flex_none()
        .h(t.s(26.))
        .px(t.s(12.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(t.radius_small)
        .text_size(t.ui_size)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label.into());
    match style {
        ButtonStyle::Primary => base
            .bg(t.accent)
            .text_color(t.accent_text)
            .hover(|s| s.bg(t.accent_hover)),
        ButtonStyle::Secondary => base
            .bg(t.input)
            .border_1()
            .border_color(t.border_strong)
            .text_color(t.text)
            .hover(|s| s.bg(t.hover)),
        ButtonStyle::Ghost => base
            .text_color(t.text_muted)
            .hover(|s| s.bg(t.hover).text_color(t.text)),
        ButtonStyle::Danger => base
            .border_1()
            .border_color(t.border_strong)
            .text_color(t.danger)
            .hover(|s| s.bg(t.hover)),
    }
}

/// A text field inside the standard field frame.
pub fn field(input: &Entity<TextInput>, t: &Theme, window: &Window, cx: &App) -> Div {
    let focused = input.read(cx).focus_handle(cx).is_focused(window);
    div()
        .h(t.s(26.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(t.radius_small)
        .bg(t.input)
        .border_1()
        .border_color(if focused { t.accent } else { t.border_strong })
        .text_size(t.ui_size)
        .text_color(t.text)
        .child(input.clone())
}

/// A small uppercase label above a group of items.
pub fn caption(text: impl Into<SharedString>, t: &Theme) -> Div {
    div()
        .text_size(t.s(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_faint)
        .child(text.into().to_uppercase())
}

/// A colored dot, for connection tags and states.
pub fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// A centered note for an empty panel: a title and an optional line below.
pub fn empty_state(title: impl Into<SharedString>, detail: Option<SharedString>, t: &Theme) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .p(px(16.))
        .child(
            div()
                .text_size(t.ui_size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_muted)
                .child(title.into()),
        )
        .children(detail.map(|d| {
            div()
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .text_center()
                .child(d)
        }))
}

/// One entry of a popup menu.
pub struct MenuItem {
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub enabled: bool,
    pub checked: bool,
    /// Drawn in the danger color, for an item that removes something.
    pub danger: bool,
    /// A line between groups instead of an item.
    pub separator: bool,
    /// Draws the label in this font family, so a font menu previews each font.
    pub font: Option<SharedString>,
    pub on_click: Box<dyn Fn(&mut Window, &mut App) + 'static>,
}

impl MenuItem {
    pub fn new(
        label: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            detail: None,
            enabled: true,
            checked: false,
            danger: false,
            separator: false,
            font: None,
            on_click: Box::new(f),
        }
    }

    /// A line between groups of items.
    pub fn separator() -> Self {
        Self {
            separator: true,
            ..Self::new("", |_, _| {})
        }
    }

    pub fn font(mut self, family: impl Into<SharedString>) -> Self {
        self.font = Some(family.into());
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// A popup menu anchored below its parent. The owner keeps whether it is open and closes
/// it from `on_dismiss`, which runs on a click outside or after an item is chosen.
pub fn menu(
    id: impl Into<ElementId>,
    items: Vec<MenuItem>,
    min_width: f32,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    t: &Theme,
) -> AnyElement {
    let list = menu_list(id, items, min_width, on_dismiss, t);
    gpui::deferred(
        gpui::anchored()
            .snap_to_window()
            .child(div().mt(px(2.)).child(list)),
    )
    .with_priority(1)
    .into_any_element()
}

/// A popup menu at `position` in the window, as a right click opens. It can go anywhere
/// in the owner's tree, and it closes as [`menu`] does.
pub fn context_menu(
    id: impl Into<ElementId>,
    items: Vec<MenuItem>,
    position: gpui::Point<gpui::Pixels>,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    t: &Theme,
) -> AnyElement {
    let list = menu_list(id, items, 220., on_dismiss, t).max_h(px(680.));
    gpui::deferred(
        gpui::anchored()
            .position(position)
            .snap_to_window_with_margin(px(4.))
            .child(list),
    )
    .with_priority(2)
    .into_any_element()
}

/// Separators that would open or close a menu, or sit next to each other, are dropped.
fn tidy(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for item in items {
        if item.separator && out.last().is_none_or(|l| l.separator) {
            continue;
        }
        out.push(item);
    }
    while out.last().is_some_and(|l| l.separator) {
        out.pop();
    }
    out
}

fn menu_list(
    id: impl Into<ElementId>,
    items: Vec<MenuItem>,
    min_width: f32,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    t: &Theme,
) -> gpui::Stateful<Div> {
    let dismiss = std::rc::Rc::new(on_dismiss);
    let outside = dismiss.clone();
    let mut list = div()
        .id(id.into())
        .occlude()
        .min_w(px(min_width))
        .max_h(px(480.))
        .overflow_y_scroll()
        .py(px(4.))
        .rounded(t.radius)
        .bg(t.floating)
        .border_1()
        .border_color(t.border_strong)
        .shadow_lg()
        .font_family(t.ui_font.clone())
        .text_size(t.ui_size)
        .on_mouse_down_out(move |_, window, cx| outside(window, cx));
    for (ix, item) in tidy(items).into_iter().enumerate() {
        let dismiss = dismiss.clone();
        let MenuItem {
            label,
            detail,
            enabled,
            checked,
            danger,
            separator,
            font,
            on_click,
        } = item;
        if separator {
            list = list.child(div().mx(px(8.)).my(px(4.)).h(px(1.)).bg(t.border));
            continue;
        }
        let color = match (enabled, danger) {
            (false, _) => t.text_faint,
            (true, true) => t.danger,
            (true, false) => t.text,
        };
        let hover_bg = if danger { t.danger } else { t.accent };
        list = list.child(
            div()
                .id(ix)
                .mx(px(4.))
                .h(t.s(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(t.radius_small)
                .text_color(color)
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|s| s.bg(hover_bg).text_color(t.accent_text))
                        .on_click(move |_, window, cx| {
                            on_click(window, cx);
                            dismiss(window, cx);
                        })
                })
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .when(checked, |el| el.child(icon(Icon::Check).size(px(12.)))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .when_some(font, |el, f| el.font_family(f))
                        .child(label),
                )
                .children(detail.map(|d| div().text_size(t.small_size).opacity(0.7).child(d))),
        );
    }
    list
}

/// A segmented control: one button per option, the current one raised.
pub fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(&'static str, T)],
    current: T,
    on_pick: impl Fn(T, &mut Window, &mut App) + 'static,
    t: &Theme,
) -> Div {
    let on_pick = std::rc::Rc::new(on_pick);
    let mut row = div()
        .flex()
        .flex_none()
        .p(px(2.))
        .gap(px(2.))
        .rounded(t.radius_small)
        .bg(t.base)
        .border_1()
        .border_color(t.border);
    for (ix, (label, value)) in options.iter().copied().enumerate() {
        let selected = value == current;
        let pick = on_pick.clone();
        row = row.child(
            div()
                .id(ElementId::NamedInteger(id.into(), ix as u64))
                .h(t.s(20.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(t.radius_small)
                .text_size(t.small_size)
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(selected, |el| {
                    el.bg(t.surface).text_color(t.text).shadow_sm()
                })
                .when(!selected, |el| {
                    el.text_color(t.text_muted).hover(|s| s.text_color(t.text))
                })
                .child(label)
                .on_click(move |_, window, cx| pick(value, window, cx)),
        );
    }
    row
}

/// An on/off switch.
pub fn switch(id: impl Into<ElementId>, on: bool, t: &Theme) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { t.accent } else { t.border_strong })
        .flex()
        .when(on, |el| el.justify_end())
        .child(
            div()
                .size(px(14.))
                .rounded_full()
                .bg(gpui::white())
                .shadow_sm(),
        )
}

/// `48210331` as `48,210,331`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A short row count for the sidebar: `12.5K`, `48M`.
pub fn compact_count(n: u64) -> String {
    match n {
        0..=9_999 => thousands(n),
        10_000..=999_999 => format!("{:.0}K", n as f64 / 1_000.0),
        1_000_000..=9_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
        10_000_000..=999_999_999 => format!("{:.0}M", n as f64 / 1_000_000.0),
        _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_naturally() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(48_210_331), "48,210,331");
        assert_eq!(compact_count(1_250), "1,250");
        assert_eq!(compact_count(12_480), "12K");
        assert_eq!(compact_count(2_000_000), "2.0M");
        assert_eq!(compact_count(50_000_000), "50M");
    }
}
