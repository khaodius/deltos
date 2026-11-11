//! Settings, grouped into sections down the left. Every change applies at once and is
//! written to `settings.toml` a moment later.

use deltos_settings::{
    Appearance, DateFormat, Density, HeaderStyle, KEYBIND_ACTIONS, NullStyle, Palette, Settings,
    TabStyle, ThemeMode, TitleBarStyle,
};
use gpui::{
    AnyElement, App, AppContext, Context, Div, ElementId, Entity, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::state::{AppState, StateEvent};
use crate::theme::{
    BUNDLED_MONO_FONTS, BUNDLED_UI_FONTS, DEFAULT_GRID_FONT, Theme, color, colors, theme,
};
use crate::ui::color_picker::{ColorEvent, ColorPicker};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, button, field, menu, segmented, switch};
use crate::update::{CURRENT, UpdateStatus, Updates};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Data,
    Editor,
    Updates,
    Keybindings,
    About,
}

impl Section {
    pub const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Data,
        Self::Editor,
        Self::Updates,
        Self::Keybindings,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Data => "Data",
            Self::Editor => "Editor",
            Self::Updates => "Updates",
            Self::Keybindings => "Keyboard",
            Self::About => "About",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Self::Appearance => Icon::Settings,
            Self::Data => Icon::Grid,
            Self::Editor => Icon::Code,
            Self::Updates => Icon::Refresh,
            Self::Keybindings => Icon::Terminal,
            Self::About => Icon::Logo,
        }
    }
}

/// Accent presets, as 0xRRGGBB.
const ACCENTS: [u32; 12] = [
    0x2F7CF6, 0x4F5BD5, 0x7C5CF6, 0xB45AF2, 0xD9467A, 0xE5484D, 0xEE7D2B, 0xD4A20F, 0x2E9F5E,
    0x12A0A0, 0x0EA5E9, 0x6E6E73,
];

/// Accent tint steps: off, subtle, medium, strong.
/// The grid font menu's first entries: the default, then the fonts Deltos ships.
const GRID_FONTS: [&str; 5] = [
    DEFAULT_GRID_FONT,
    "Adwaita Sans",
    "Inter",
    "Go Mono",
    "JetBrains Mono",
];

const TINTS: [(&str, u32); 4] = [("Off", 0), ("Subtle", 5), ("Medium", 10), ("Strong", 18)];

/// Which font a font menu sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontTarget {
    Ui,
    Mono,
    Grid,
    Editor,
}

pub struct SettingsPanel {
    state: Entity<AppState>,
    section: Section,
    accent_hex: Entity<TextInput>,
    accent_picker: Entity<ColorPicker>,
    font_menu: Option<FontTarget>,
    /// The system's font families, read when a font menu first opens.
    system_fonts: Option<Vec<String>>,
    keys: Vec<(&'static str, Entity<TextInput>)>,
    theme_menu: bool,
    _subs: Vec<Subscription>,
}

impl SettingsPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let s = state.read(cx).settings.clone();
        let text_input =
            |placeholder: &'static str, text: Option<String>, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut i = TextInput::new(placeholder, cx);
                    if let Some(t) = text {
                        i.set_text(t, cx);
                    }
                    i
                })
            };
        let accent_hex = text_input("#2F7CF6", Some(format!("#{:06X}", s.appearance.accent)), cx);
        let keys: Vec<_> = KEYBIND_ACTIONS
            .iter()
            .map(|(action, default, _)| {
                (
                    *action,
                    text_input(default, s.keybinds.get(action).map(str::to_owned), cx),
                )
            })
            .collect();
        let accent_picker = cx.new(|cx| ColorPicker::new(s.appearance.accent, cx));
        let mut subs = vec![cx.subscribe(&state, |this, state, e, cx| {
            if matches!(e, StateEvent::Settings | StateEvent::Update) {
                // A swatch, the hex field or a reset moved the accent: the picker follows.
                let accent = state.read(cx).settings.appearance.accent;
                this.accent_picker.update(cx, |p, cx| p.set_rgb(accent, cx));
                cx.notify();
            }
        })];
        subs.push(cx.subscribe(&accent_picker, |this, _, e, cx| {
            let ColorEvent::Changed(c) = *e;
            this.accent_hex
                .update(cx, |i, cx| i.set_text(format!("#{c:06X}"), cx));
            this.set(cx, move |s| s.appearance.accent = c);
        }));
        // A typed accent applies on Enter; anything that is not a color puts the current
        // one back.
        subs.push(cx.subscribe(&accent_hex, |this, input, e, cx| {
            if matches!(e, InputEvent::Submit | InputEvent::Cancel) {
                let parsed = deltos_settings::parse_color(input.read(cx).text());
                if let (InputEvent::Submit, Some(rgba)) = (e, parsed) {
                    this.set(cx, move |s| s.appearance.accent = rgba >> 8);
                }
                let current = this.state.read(cx).settings.appearance.accent;
                input.update(cx, |i, cx| i.set_text(format!("#{current:06X}"), cx));
            }
        }));
        for (action, input) in &keys {
            let action = *action;
            subs.push(cx.subscribe(input, move |this, input, e, cx| match e {
                InputEvent::Submit => {
                    let keys = input.read(cx).text().trim().to_lowercase();
                    this.set(cx, |s| {
                        s.keybinds.0.insert(action.to_owned(), keys);
                    });
                    let settings = this.state.read(cx).settings.clone();
                    crate::rebind_keys(&settings, cx);
                }
                InputEvent::Cancel => {
                    let current = this
                        .state
                        .read(cx)
                        .settings
                        .keybinds
                        .get(action)
                        .unwrap_or("")
                        .to_owned();
                    input.update(cx, |i, cx| i.set_text(current, cx));
                }
                InputEvent::Changed => {}
            }));
        }
        if let Some(Updates(u)) = cx.try_global::<Updates>() {
            let u = u.clone();
            subs.push(cx.observe(&u, |_, _, cx| cx.notify()));
        }
        Self {
            state,
            section: Section::Appearance,
            accent_hex,
            accent_picker,
            font_menu: None,
            system_fonts: None,
            keys,
            theme_menu: false,
            _subs: subs,
        }
    }

    /// Shows one section, as the command palette's settings pages ask.
    pub fn show_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    fn set(&self, cx: &mut App, f: impl FnOnce(&mut Settings)) {
        self.state.update(cx, |s, cx| s.update_settings(cx, f));
    }

    /// The font picker for one setting: the fonts Deltos ships first, then the system's.
    fn font_picker(
        &mut self,
        target: FontTarget,
        current: Option<String>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (bundled, default): (&[&str], &str) = match target {
            FontTarget::Ui => (&BUNDLED_UI_FONTS, BUNDLED_UI_FONTS[0]),
            FontTarget::Mono => (&BUNDLED_MONO_FONTS, BUNDLED_MONO_FONTS[0]),
            FontTarget::Grid => (&GRID_FONTS, GRID_FONTS[0]),
            FontTarget::Editor => (&BUNDLED_MONO_FONTS, "Same as the code font"),
        };
        let shown: SharedString = current.clone().unwrap_or_else(|| default.into()).into();
        let id: &'static str = match target {
            FontTarget::Ui => "font-ui",
            FontTarget::Mono => "font-mono",
            FontTarget::Grid => "font-grid",
            FontTarget::Editor => "font-editor",
        };
        let family = match target {
            FontTarget::Ui => t.ui_font.clone(),
            FontTarget::Mono => t.mono_font.clone(),
            FontTarget::Grid => t.grid_font.clone(),
            FontTarget::Editor => t.editor_font.clone(),
        };
        let mut picker = div()
            .id(id)
            .relative()
            .w(px(200.))
            .h(t.s(26.))
            .px(px(8.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .rounded(t.radius_small)
            .bg(t.input)
            .border_1()
            .border_color(t.border_strong)
            .cursor_pointer()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(family)
                    .child(shown),
            )
            .child(
                icon(Icon::ChevronDown)
                    .size(px(12.))
                    .text_color(t.text_faint),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.system_fonts.is_none() {
                    let mut names = cx.text_system().all_font_names();
                    names.sort_by_key(|n| n.to_lowercase());
                    names.dedup();
                    this.system_fonts = Some(names);
                }
                this.font_menu = Some(target);
                cx.notify();
            }));
        if self.font_menu == Some(target) {
            let weak = cx.entity().downgrade();
            let choose = |value: Option<String>| {
                let w = weak.clone();
                move |_: &mut Window, cx: &mut App| {
                    let value = value.clone();
                    w.update(cx, |this, cx| {
                        this.set(cx, move |s| match target {
                            FontTarget::Ui => s.appearance.ui_font = value,
                            FontTarget::Mono => s.appearance.mono_font = value,
                            FontTarget::Grid => s.appearance.grid_font = value,
                            FontTarget::Editor => s.editor.font = value,
                        })
                    })
                    .ok();
                }
            };
            let mut items = Vec::new();
            if target == FontTarget::Editor {
                items.push(
                    MenuItem::new("Same as the code font", choose(None)).checked(current.is_none()),
                );
            }
            for (i, name) in bundled.iter().enumerate() {
                // The first bundled font is the default, kept as no setting at all.
                let value = (target == FontTarget::Editor || i > 0).then(|| (*name).to_owned());
                let checked = match (&current, &value) {
                    (None, None) => true,
                    (Some(c), _) => c == name,
                    _ => false,
                };
                let detail = if target == FontTarget::Grid && i == 0 {
                    "default"
                } else {
                    "built in"
                };
                items.push(
                    MenuItem::new(*name, choose(value))
                        .checked(checked)
                        .detail(detail)
                        .font(*name),
                );
            }
            for name in self.system_fonts.iter().flatten() {
                if bundled.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                items.push(
                    MenuItem::new(name.clone(), choose(Some(name.clone())))
                        .checked(current.as_deref() == Some(name.as_str()))
                        .font(name.clone()),
                );
            }
            let close = weak.clone();
            picker = picker.child(menu(
                ("font-menu", target as usize),
                items,
                220.,
                move |_, cx| {
                    close
                        .update(cx, |this, cx| {
                            this.font_menu = None;
                            cx.notify();
                        })
                        .ok();
                },
                t,
            ));
        }
        picker.into_any_element()
    }

    /// One tile per palette on one side, each a small picture of the window in it.
    fn palette_tiles(
        &self,
        dark: bool,
        current: Palette,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = t.accent;
        let list: &[Palette] = if dark {
            &Palette::DARK
        } else {
            &Palette::LIGHT
        };
        let mut row = div().flex().flex_wrap().gap(px(8.));
        for (i, palette) in list.iter().copied().enumerate() {
            let c = colors(palette);
            let on = palette == current;
            let w = cx.entity().downgrade();
            row = row.child(
                div()
                    .id((if dark { "pal-dark" } else { "pal-light" }, i))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        w.update(cx, |this, cx| {
                            this.set(cx, move |s| {
                                if dark {
                                    s.appearance.dark_palette = palette;
                                } else {
                                    s.appearance.light_palette = palette;
                                }
                            })
                        })
                        .ok();
                    })
                    .child(
                        div()
                            .w(px(66.))
                            .h(px(44.))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .border_2()
                            .border_color(if on { accent } else { color(c.border) })
                            .bg(color(c.base))
                            .flex()
                            .flex_col()
                            .child(div().h(px(7.)).bg(color(c.title_bar)))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .child(div().w(px(16.)).h_full().bg(color(c.sidebar)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .h_full()
                                            .bg(color(c.surface))
                                            .p(px(4.))
                                            .flex()
                                            .flex_col()
                                            .gap(px(3.))
                                            .child(
                                                div()
                                                    .w(px(26.))
                                                    .h(px(3.))
                                                    .rounded(px(1.))
                                                    .bg(color(c.text)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(18.))
                                                    .h(px(3.))
                                                    .rounded(px(1.))
                                                    .bg(color(c.text_muted)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(12.))
                                                    .h(px(3.))
                                                    .rounded(px(1.))
                                                    .bg(accent),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(t.small_size)
                            .text_color(if on { t.text } else { t.text_muted })
                            .child(palette.label()),
                    ),
            );
        }
        row.into_any_element()
    }

    /// A small picture of the window in the current settings, so each change shows at
    /// once without leaving the page.
    fn preview(&self, a: &Appearance, t: &Theme) -> AnyElement {
        let bar = div()
            .h(t.s(22.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(t.title_bar)
            .border_b_1()
            .border_color(t.border)
            .when_some(t.title_bar_line, |el, line| {
                el.border_b_2().border_color(line)
            })
            .child(icon(Icon::Logo).size(px(11.)).text_color(t.accent))
            .child(
                div()
                    .text_size(t.small_size)
                    .text_color(t.text_muted)
                    .child("Deltos"),
            )
            .child(div().flex_1())
            .child(div().size(px(7.)).rounded_full().bg(t.text_faint))
            .child(div().size(px(7.)).rounded_full().bg(t.text_faint))
            .child(div().size(px(7.)).rounded_full().bg(t.danger));
        let tab = |label: &'static str, active: bool| {
            let el = div()
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(5.))
                .text_size(t.small_size)
                .text_color(if active { t.text } else { t.text_muted })
                .child(icon(Icon::Table).size(px(11.)).text_color(if active {
                    t.accent
                } else {
                    t.text_faint
                }))
                .child(label);
            match (t.tab_style, active) {
                (TabStyle::Card, true) => el
                    .h_full()
                    .bg(t.surface)
                    .border_r_1()
                    .border_color(t.border),
                (TabStyle::Card, false) => el.h_full().border_r_1().border_color(t.border),
                (TabStyle::Underline, true) => el.h_full().border_b_2().border_color(t.accent),
                (TabStyle::Underline, false) => el.h_full(),
                (TabStyle::Pill, true) => {
                    el.h(t.s(18.)).mx(px(2.)).rounded(t.radius).bg(t.selected)
                }
                (TabStyle::Pill, false) => el.h(t.s(18.)).mx(px(2.)),
            }
        };
        let tabs = div()
            .h(t.s(24.))
            .flex()
            .items_center()
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .child(tab("orders", true))
            .child(tab("customers", false));
        let rows: [[&str; 3]; 4] = [
            ["1001", "Ada Lovelace", "2024-03-09"],
            ["1002", "Grace Hopper", ""],
            ["1003", "Edsger Dijkstra", "2024-03-11"],
            ["1004", "Barbara Liskov", "2024-03-12"],
        ];
        let cell = |text: SharedString, w: f32, numeric: bool, color: gpui::Hsla| {
            div()
                .w(px(w))
                .h_full()
                .px(px(6.))
                .flex()
                .items_center()
                .when(numeric, |el| el.justify_end())
                .when(t.grid_lines, |el| el.border_r_1().border_color(t.grid_line))
                .text_color(color)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(text)
        };
        let widths = [52., 132., 96.];
        let mut header = div()
            .h(t.row_height)
            .flex()
            .bg(t.grid_header)
            .border_b_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .font_weight(t.header_weight());
        for (name, w) in ["id", "name", "joined"].iter().zip(widths) {
            header = header.child(cell(t.header_text(name), w, *name == "id", t.text_muted));
        }
        let mut grid = div()
            .flex()
            .flex_col()
            .font_family(t.mono_font.clone())
            .text_size(t.mono_size)
            .child(header);
        for (i, r) in rows.iter().enumerate() {
            let selected = i == 2;
            let mut line = div()
                .h(t.row_height)
                .flex()
                .when(t.zebra && i % 2 == 1, |el| el.bg(t.surface_alt))
                .when(selected, |el| el.bg(t.selected_row))
                .when(t.grid_lines, |el| el.border_b_1().border_color(t.grid_line));
            for (j, (v, w)) in r.iter().zip(widths).enumerate() {
                let (text, c): (SharedString, _) = if v.is_empty() {
                    (t.null_text.into(), t.text_faint)
                } else {
                    ((*v).into(), t.text)
                };
                line = line.child(cell(text, w, j == 0, c));
            }
            grid = grid.child(line);
        }
        let sidebar = div()
            .w(px(104.))
            .h_full()
            .flex_none()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border)
            .p(px(4.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .text_size(t.small_size)
            .children(["customers", "orders", "products"].iter().map(|name| {
                let on = *name == "orders";
                div()
                    .h(t.list_row)
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .rounded(t.radius_small)
                    .when(on, |el| el.bg(t.selected))
                    .text_color(if on { t.text } else { t.text_muted })
                    .child(icon(Icon::Table).size(px(11.)).text_color(if on {
                        t.accent
                    } else {
                        t.text_faint
                    }))
                    .child(*name)
            }));
        let footer = div()
            .px(px(8.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .border_t_1()
            .border_color(t.border)
            .child(
                div()
                    .h(t.s(20.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_small)
                    .bg(t.accent)
                    .text_color(t.accent_text)
                    .text_size(t.small_size)
                    .child("Run"),
            )
            .child(
                div()
                    .font_family(t.editor_font.clone())
                    .text_size(t.small_size)
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .child(div().text_color(t.syntax_keyword).child("SELECT"))
                            .child(div().text_color(t.text).child("*"))
                            .child(div().text_color(t.syntax_keyword).child("FROM"))
                            .child(div().text_color(t.text).child("orders"))
                            .child(div().text_color(t.syntax_keyword).child("LIMIT"))
                            .child(div().text_color(t.syntax_number).child("50")),
                    ),
            );
        div()
            .mb(px(18.))
            .rounded(t.radius)
            .overflow_hidden()
            .border_1()
            .border_color(t.border_strong)
            .bg(t.base)
            .when(a.window_opacity < 0.999, |el| {
                el.opacity(a.window_opacity.max(0.5))
            })
            .font_family(t.ui_font.clone())
            .child(bar)
            .child(
                div()
                    .h(t.row_height * 5. + t.s(24.))
                    .flex()
                    .child(sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .bg(t.surface)
                            .child(tabs)
                            .child(grid),
                    ),
            )
            .child(div().bg(t.surface).child(footer))
            .into_any_element()
    }

    fn appearance(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let st = self.state.read(cx);
        let a = st.settings.appearance.clone();
        let themes: Vec<String> = st.themes.iter().map(|(n, _)| n.clone()).collect();
        let w = cx.entity().downgrade();
        let pick = move |f: fn(&mut Settings)| {
            let w = w.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| this.set(cx, f)).ok();
            }
        };
        let modes: Vec<_> = ThemeMode::ALL.iter().map(|m| (m.label(), *m)).collect();
        let densities: Vec<_> = Density::ALL.iter().map(|d| (d.label(), *d)).collect();
        let nulls: Vec<_> = NullStyle::ALL.iter().map(|n| (n.label(), *n)).collect();
        let dates: Vec<_> = DateFormat::ALL.iter().map(|d| (d.label(), *d)).collect();
        let bars: Vec<_> = TitleBarStyle::ALL.iter().map(|b| (b.label(), *b)).collect();
        let tabs: Vec<_> = TabStyle::ALL.iter().map(|b| (b.label(), *b)).collect();
        let headers: Vec<_> = HeaderStyle::ALL.iter().map(|b| (b.label(), *b)).collect();
        let state = self.state.clone();
        let theme_control = {
            let current: SharedString = a
                .custom_theme
                .clone()
                .unwrap_or_else(|| "Built-in".into())
                .into();
            let mut picker = div()
                .id("theme-file")
                .relative()
                .min_w(px(160.))
                .h(t.s(26.))
                .px(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .rounded(t.radius_small)
                .bg(t.input)
                .border_1()
                .border_color(t.border_strong)
                .cursor_pointer()
                .child(current)
                .child(
                    icon(Icon::ChevronDown)
                        .size(px(12.))
                        .text_color(t.text_faint),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.theme_menu = true;
                    cx.notify();
                }));
            if self.theme_menu {
                let weak = cx.entity().downgrade();
                let mut items = vec![{
                    let w = weak.clone();
                    MenuItem::new("Built-in", move |_, cx| {
                        w.update(cx, |this, cx| {
                            this.set(cx, |s| s.appearance.custom_theme = None)
                        })
                        .ok();
                    })
                    .checked(a.custom_theme.is_none())
                }];
                for name in &themes {
                    let (w, n) = (weak.clone(), name.clone());
                    items.push(
                        MenuItem::new(name.clone(), move |_, cx| {
                            let n = n.clone();
                            w.update(cx, |this, cx| {
                                this.set(cx, move |s| s.appearance.custom_theme = Some(n))
                            })
                            .ok();
                        })
                        .checked(a.custom_theme.as_deref() == Some(name.as_str())),
                    );
                }
                let w = weak.clone();
                items.push(MenuItem::new("Rescan the themes folder", move |_, cx| {
                    w.update(cx, |this, cx| {
                        this.state.update(cx, |s, cx| s.rescan_themes(cx))
                    })
                    .ok();
                }));
                let close = weak.clone();
                picker = picker.child(menu(
                    "theme-menu",
                    items,
                    200.,
                    move |_, cx| {
                        close
                            .update(cx, |this, cx| {
                                this.theme_menu = false;
                                cx.notify();
                            })
                            .ok();
                    },
                    t,
                ));
            }
            picker
        };
        let mut swatches = div().flex().flex_wrap().gap(px(6.)).max_w(px(240.));
        for (i, c) in ACCENTS.iter().copied().enumerate() {
            let selected = a.accent == c;
            let w = cx.entity().downgrade();
            let hex = self.accent_hex.clone();
            swatches = swatches.child(
                div()
                    .id(("accent", i))
                    .size(px(18.))
                    .rounded_full()
                    .bg(color(c))
                    .cursor_pointer()
                    .border_2()
                    .border_color(if selected {
                        t.text
                    } else {
                        gpui::transparent_black()
                    })
                    .on_click(move |_, _, cx| {
                        hex.update(cx, |i, cx| i.set_text(format!("#{c:06X}"), cx));
                        w.update(cx, |this, cx| this.set(cx, |s| s.appearance.accent = c))
                            .ok();
                    }),
            );
        }
        let accent_control = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(swatches)
                    .child(
                        div()
                            .w(px(84.))
                            .child(field(&self.accent_hex, t, window, cx)),
                    ),
            )
            .child(self.accent_picker.clone());
        let tint_now = (a.accent_tint * 100.0).round() as u32;
        let tint_current = TINTS
            .iter()
            .map(|(_, v)| *v)
            .min_by_key(|v| v.abs_diff(tint_now))
            .unwrap_or(0);
        let ui_picker = self.font_picker(FontTarget::Ui, a.ui_font.clone(), t, cx);
        let mono_picker = self.font_picker(FontTarget::Mono, a.mono_font.clone(), t, cx);
        let grid_picker = self.font_picker(FontTarget::Grid, a.grid_font.clone(), t, cx);
        let light_tiles = self.palette_tiles(false, a.light_palette, t, cx);
        let dark_tiles = self.palette_tiles(true, a.dark_palette, t, cx);
        let preview = self.preview(&a, t);
        let (s1, s2, s3, s4, s5, s6, s7, s8) = (
            state.clone(),
            state.clone(),
            state.clone(),
            state.clone(),
            state.clone(),
            state.clone(),
            state.clone(),
            state.clone(),
        );
        let hex_reset = self.accent_hex.clone();
        group("Theme", t)
            .child(preview)
            .child(row(
                "Appearance",
                Some("Follow the system, or always light or dark."),
                segmented(
                    "theme-mode",
                    &modes,
                    a.theme,
                    move |m, _, cx| {
                        s1.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.theme = m)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(stacked_row(
                "Light palette",
                Some("Used in light mode."),
                light_tiles,
                t,
            ))
            .child(stacked_row(
                "Dark palette",
                Some("Used in dark mode."),
                dark_tiles,
                t,
            ))
            .child(stacked_row(
                "Accent",
                Some("Pick one, drag in the square and the hue bar, take one from the screen with the eyedropper, or type a hex color and press Enter."),
                accent_control.into_any_element(),
                t,
            ))
            .child(row(
                "Accent tint",
                Some("Washes the accent into the window's surfaces."),
                segmented(
                    "tint",
                    &TINTS,
                    tint_current,
                    move |v, _, cx| {
                        s5.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.accent_tint = v as f32 / 100.0)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Title bar",
                None,
                segmented(
                    "bar-style",
                    &bars,
                    a.title_bar_style,
                    move |v, _, cx| {
                        s6.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.title_bar_style = v)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Tabs",
                None,
                segmented(
                    "tab-style",
                    &tabs,
                    a.tab_style,
                    move |v, _, cx| {
                        s7.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.tab_style = v)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Theme file",
                Some("Colors from a .toml file in the themes folder, over the palette."),
                theme_control.into_any_element(),
                t,
            ))
            .child(toggle_row(
                "Window title bar",
                Some("Draw Deltos' own title bar instead of the system's."),
                ("title-bar", a.custom_title_bar),
                pick(|s| s.appearance.custom_title_bar = !s.appearance.custom_title_bar),
                t,
            ))
            .child(row(
                "Window opacity",
                Some("Only some systems honor it."),
                stepper(
                    "opacity",
                    format!("{:.0}%", a.window_opacity * 100.0),
                    pick(|s| s.appearance.window_opacity -= 0.05),
                    pick(|s| s.appearance.window_opacity += 0.05),
                    t,
                ),
                t,
            ))
            .child(gap())
            .child(heading("Text", t))
            .child(row(
                "Interface zoom",
                None,
                stepper(
                    "scale",
                    format!("{:.0}%", a.ui_scale * 100.0),
                    pick(|s| s.appearance.ui_scale -= 0.05),
                    pick(|s| s.appearance.ui_scale += 0.05),
                    t,
                ),
                t,
            ))
            .child(row("Interface font", None, ui_picker, t))
            .child(row(
                "Interface size",
                None,
                stepper(
                    "ui-size",
                    format!("{:.0} px", a.ui_font_size),
                    pick(|s| s.appearance.ui_font_size -= 1.0),
                    pick(|s| s.appearance.ui_font_size += 1.0),
                    t,
                ),
                t,
            ))
            .child(row(
                "Grid font",
                Some("Malgun Gothic by default, or the interface font where it is missing."),
                grid_picker,
                t,
            ))
            .child(row(
                "Code font",
                Some("The query editor, the inspector and other aligned text."),
                mono_picker,
                t,
            ))
            .child(row(
                "Grid size",
                None,
                stepper(
                    "mono-size",
                    format!("{:.0} px", a.mono_font_size),
                    pick(|s| s.appearance.mono_font_size -= 1.0),
                    pick(|s| s.appearance.mono_font_size += 1.0),
                    t,
                ),
                t,
            ))
            .child(toggle_row(
                "High contrast text",
                Some("Brighter secondary text and labels."),
                ("contrast-text", a.high_contrast),
                pick(|s| s.appearance.high_contrast = !s.appearance.high_contrast),
                t,
            ))
            .child(gap())
            .child(heading("Grid", t))
            .child(row(
                "Density",
                Some("Row height in grids and lists."),
                segmented(
                    "density",
                    &densities,
                    a.density,
                    move |d, _, cx| {
                        s2.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.density = d)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Column headers",
                None,
                segmented(
                    "header-style",
                    &headers,
                    a.header_style,
                    move |v, _, cx| {
                        s8.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.header_style = v)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(toggle_row(
                "Striped rows",
                None,
                ("zebra", a.zebra_stripes),
                pick(|s| s.appearance.zebra_stripes = !s.appearance.zebra_stripes),
                t,
            ))
            .child(toggle_row(
                "Cell lines",
                None,
                ("grid-lines", a.grid_lines),
                pick(|s| s.appearance.grid_lines = !s.appearance.grid_lines),
                t,
            ))
            .child(row(
                "NULL values",
                None,
                segmented(
                    "nulls",
                    &nulls,
                    a.null_style,
                    move |n, _, cx| {
                        s3.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.null_style = n)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Dates",
                None,
                segmented(
                    "dates",
                    &dates,
                    a.date_format,
                    move |d, _, cx| {
                        s4.update(cx, |s, cx| {
                            s.update_settings(cx, |st| st.appearance.date_format = d)
                        })
                    },
                    t,
                )
                .into_any_element(),
                t,
            ))
            .child(row(
                "Corner radius",
                None,
                stepper(
                    "radius",
                    format!("{:.0} px", a.corner_radius),
                    pick(|s| s.appearance.corner_radius -= 1.0),
                    pick(|s| s.appearance.corner_radius += 1.0),
                    t,
                ),
                t,
            ))
            .child(row(
                "Separators",
                Some("How strong lines between areas are."),
                stepper(
                    "contrast",
                    format!("{:.0}%", a.border_contrast * 100.0),
                    pick(|s| s.appearance.border_contrast -= 0.25),
                    pick(|s| s.appearance.border_contrast += 0.25),
                    t,
                ),
                t,
            ))
            .child(toggle_row(
                "Row counts in the sidebar",
                None,
                ("sidebar-counts", a.sidebar_row_counts),
                pick(|s| s.appearance.sidebar_row_counts = !s.appearance.sidebar_row_counts),
                t,
            ))
            .child(
                div().pt(px(14.)).child(
                    button(
                        "appearance-reset",
                        "Restore default appearance",
                        ButtonStyle::Secondary,
                        t,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set(cx, |s| {
                            // The title bar setting is about the window, not the look, so
                            // it stays as the person set it.
                            let own_bar = s.appearance.custom_title_bar;
                            s.appearance = Appearance {
                                custom_title_bar: own_bar,
                                ..Appearance::default()
                            };
                        });
                        let accent = Appearance::default().accent;
                        hex_reset.update(cx, |i, cx| i.set_text(format!("#{accent:06X}"), cx));
                    })),
                ),
            )
    }

    fn data(&mut self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let d = self.state.read(cx).settings.data.clone();
        let w = cx.entity().downgrade();
        let pick = move |f: fn(&mut Settings)| {
            let w = w.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| this.set(cx, f)).ok();
            }
        };
        group("Loading", t)
            .child(row(
                "Rows per page",
                Some("Rows read per request while scrolling."),
                stepper(
                    "page-size",
                    crate::ui::thousands(u64::from(d.page_size)),
                    pick(|s| {
                        s.data.page_size =
                            step_down(s.data.page_size, &[50, 100, 250, 500, 1000, 2000, 5000])
                    }),
                    pick(|s| {
                        s.data.page_size =
                            step_up(s.data.page_size, &[50, 100, 250, 500, 1000, 2000, 5000])
                    }),
                    t,
                ),
                t,
            ))
            .child(row(
                "Memory per table",
                Some("Rows kept in memory for each open table; older pages are dropped."),
                stepper(
                    "cache-mb",
                    format!("{} MB", d.cache_memory_mb),
                    pick(|s| {
                        s.data.cache_memory_mb = step_down(
                            s.data.cache_memory_mb,
                            &[32, 64, 128, 256, 512, 1024, 2048, 4096],
                        )
                    }),
                    pick(|s| {
                        s.data.cache_memory_mb = step_up(
                            s.data.cache_memory_mb,
                            &[32, 64, 128, 256, 512, 1024, 2048, 4096],
                        )
                    }),
                    t,
                ),
                t,
            ))
            .child(row(
                "Query results on disk",
                Some("Results larger than memory are kept in a temporary file, up to this size."),
                stepper(
                    "spill-mb",
                    if d.spill_limit_mb == 0 {
                        "No limit".into()
                    } else {
                        format!("{} GB", d.spill_limit_mb / 1024)
                    },
                    pick(|s| {
                        s.data.spill_limit_mb = step_down(
                            s.data.spill_limit_mb,
                            &[1024, 2048, 4096, 8192, 16384, 65536, 0],
                        )
                    }),
                    pick(|s| {
                        s.data.spill_limit_mb = step_up(
                            s.data.spill_limit_mb,
                            &[1024, 2048, 4096, 8192, 16384, 65536, 0],
                        )
                    }),
                    t,
                ),
                t,
            ))
            .child(toggle_row(
                "Exact row counts",
                Some(
                    "Count every row instead of using the server's estimate. Slow on large tables.",
                ),
                ("exact", d.exact_counts),
                pick(|s| s.data.exact_counts = !s.data.exact_counts),
                t,
            ))
            .child(gap())
            .child(heading("Editing", t))
            .child(toggle_row(
                "Commit edits immediately",
                Some("Write each cell edit, deletion and duplicate to the database at once, instead of staging changes until you commit them."),
                ("commit-now", d.commit_immediately),
                pick(|s| s.data.commit_immediately = !s.data.commit_immediately),
                t,
            ))
            .child(gap())
            .child(heading("Safety", t))
            .child(toggle_row(
                "Confirm destructive statements",
                Some("Ask before DROP, TRUNCATE, or DELETE and UPDATE without WHERE."),
                ("confirm", d.confirm_destructive),
                pick(|s| s.data.confirm_destructive = !s.data.confirm_destructive),
                t,
            ))
            .child(row(
                "Statement timeout",
                Some("Cancel statements that run longer than this."),
                stepper(
                    "timeout",
                    if d.query_timeout_secs == 0 {
                        "Never".into()
                    } else {
                        format!("{} s", d.query_timeout_secs)
                    },
                    pick(|s| {
                        s.data.query_timeout_secs =
                            step_down(s.data.query_timeout_secs, &[0, 10, 30, 60, 300, 900, 3600])
                    }),
                    pick(|s| {
                        s.data.query_timeout_secs =
                            step_up(s.data.query_timeout_secs, &[0, 10, 30, 60, 300, 900, 3600])
                    }),
                    t,
                ),
                t,
            ))
    }

    fn editor(&mut self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let e = self.state.read(cx).settings.editor.clone();
        let editor_picker = self.font_picker(FontTarget::Editor, e.font.clone(), t, cx);
        let w = cx.entity().downgrade();
        let pick = move |f: fn(&mut Settings)| {
            let w = w.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| this.set(cx, f)).ok();
            }
        };
        group("Editor", t)
            .child(row("Font", None, editor_picker, t))
            .child(row(
                "Font size",
                None,
                stepper(
                    "editor-size",
                    format!("{:.0} px", e.font_size),
                    pick(|s| s.editor.font_size -= 1.0),
                    pick(|s| s.editor.font_size += 1.0),
                    t,
                ),
                t,
            ))
            .child(row(
                "Tab width",
                None,
                stepper(
                    "tab-size",
                    format!("{} spaces", e.tab_size),
                    pick(|s| s.editor.tab_size = s.editor.tab_size.saturating_sub(1)),
                    pick(|s| s.editor.tab_size = s.editor.tab_size.saturating_add(1)),
                    t,
                ),
                t,
            ))
            .child(toggle_row(
                "Line numbers",
                None,
                ("line-numbers", e.line_numbers),
                pick(|s| s.editor.line_numbers = !s.editor.line_numbers),
                t,
            ))
            .child(toggle_row(
                "Syntax colors",
                None,
                ("highlight", e.highlight),
                pick(|s| s.editor.highlight = !s.editor.highlight),
                t,
            ))
    }

    fn updates(&mut self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let u = self.state.read(cx).settings.updates.clone();
        let w = cx.entity().downgrade();
        let pick = move |f: fn(&mut Settings)| {
            let w = w.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| this.set(cx, f)).ok();
            }
        };
        let model = cx.try_global::<Updates>().map(|u| u.0.clone());
        let (status, last) = model.as_ref().map_or((UpdateStatus::Idle, None), |m| {
            let m = m.read(cx);
            (m.status.clone(), m.last_check)
        });
        let (line, tone): (String, _) = match &status {
            UpdateStatus::Idle => ("Not checked yet.".into(), t.text_muted),
            UpdateStatus::Disabled(why) => (why.clone(), t.text_muted),
            UpdateStatus::Checking => ("Checking…".into(), t.text_muted),
            UpdateStatus::UpToDate => ("Deltos is up to date.".into(), t.success),
            UpdateStatus::Available(v) => (format!("Version {v} is available."), t.accent),
            UpdateStatus::Downloading {
                version,
                done,
                total,
            } => (
                format!(
                    "Downloading {version}… {}%",
                    if *total > 0 { done * 100 / total } else { 0 }
                ),
                t.text_muted,
            ),
            UpdateStatus::Ready(v) => (
                format!("Version {v} is ready. It installs when you restart."),
                t.success,
            ),
            UpdateStatus::Installing(v) => (format!("Installing {v}…"), t.text_muted),
            UpdateStatus::Failed(e) => (e.clone(), t.danger),
        };
        let last = last
            .and_then(|at| at.elapsed().ok())
            .map(|d| match d.as_secs() {
                0..=59 => "Checked just now".to_owned(),
                s if s < 3600 => format!("Checked {} min ago", s / 60),
                s => format!("Checked {} h ago", s / 3600),
            });
        let enabled = !matches!(status, UpdateStatus::Disabled(_));
        let m1 = model.clone();
        let m2 = model.clone();
        let m3 = model;
        let action: Option<AnyElement> = match &status {
            UpdateStatus::Available(_) => Some(
                button("update-download", "Download", ButtonStyle::Primary, t)
                    .on_click(move |_, _, cx| {
                        if let Some(m) = &m1 {
                            m.update(cx, |m, cx| m.download(cx));
                        }
                    })
                    .into_any_element(),
            ),
            UpdateStatus::Ready(_) => Some(
                button(
                    "update-restart",
                    "Restart to update",
                    ButtonStyle::Primary,
                    t,
                )
                .on_click(move |_, _, cx| {
                    if let Some(m) = &m2 {
                        m.update(cx, |m, cx| m.restart_to_update(cx));
                    }
                })
                .into_any_element(),
            ),
            _ if enabled => Some(
                button("update-check", "Check now", ButtonStyle::Secondary, t)
                    .on_click(move |_, _, cx| {
                        if let Some(m) = &m3 {
                            m.update(cx, |m, cx| m.check(cx));
                        }
                    })
                    .into_any_element(),
            ),
            _ => None,
        };
        group("Updates", t)
            .child(
                div()
                    .py(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(t.ui_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text)
                                    .child(format!("Deltos {CURRENT}")),
                            )
                            .child(div().text_size(t.small_size).text_color(tone).child(line))
                            .children(last.map(|l| {
                                div()
                                    .text_size(t.small_size)
                                    .text_color(t.text_faint)
                                    .child(l)
                            })),
                    )
                    .children(action),
            )
            .child(toggle_row(
                "Check automatically",
                Some("At start and every six hours."),
                ("auto-check", u.auto_check),
                pick(|s| s.updates.auto_check = !s.updates.auto_check),
                t,
            ))
            .child(toggle_row(
                "Download automatically",
                Some("Fetch new versions in the background."),
                ("auto-download", u.auto_download),
                pick(|s| s.updates.auto_download = !s.updates.auto_download),
                t,
            ))
            .child(toggle_row(
                "Install when quitting",
                Some("Apply a downloaded version as Deltos closes."),
                ("install-quit", u.install_on_quit),
                pick(|s| s.updates.install_on_quit = !s.updates.install_on_quit),
                t,
            ))
    }

    fn keybindings(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut g = group("Keyboard", t).child(
            div()
                .pb(px(6.))
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child("Type keys like ctrl-shift-k and press Enter. Several bindings are separated by spaces."),
        );
        for (action, input) in &self.keys {
            let label = KEYBIND_ACTIONS
                .iter()
                .find(|(a, _, _)| a == action)
                .map_or(*action, |(_, _, l)| *l);
            g = g.child(row(
                label,
                None,
                div()
                    .w(px(200.))
                    .child(field(input, t, window, cx))
                    .into_any_element(),
                t,
            ));
        }
        let state = self.state.clone();
        let keys: Vec<_> = self.keys.clone();
        g.child(div().pt(px(10.)).child(
            button("keys-reset", "Restore defaults", ButtonStyle::Secondary, t).on_click(
                move |_, _, cx| {
                    state.update(cx, |s, cx| {
                        s.update_settings(cx, |st| st.keybinds = Default::default());
                    });
                    let settings = state.read(cx).settings.clone();
                    for (action, input) in &keys {
                        let k = settings.keybinds.get(action).unwrap_or("").to_owned();
                        input.update(cx, |i, cx| i.set_text(k, cx));
                    }
                    crate::rebind_keys(&settings, cx);
                },
            ),
        ))
    }

    fn about(&mut self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let paths = self.state.read(cx).paths.clone();
        let mut g = group("Deltos", t).child(
            div()
                .py(px(10.))
                .flex()
                .items_center()
                .gap(px(12.))
                .child(icon(Icon::Logo).size(px(36.)).text_color(t.accent))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(t.s(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text)
                                .child("Deltos"),
                        )
                        .child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_muted)
                                .child(format!(
                                    "Version {CURRENT} · {} {}",
                                    std::env::consts::OS,
                                    std::env::consts::ARCH
                                )),
                        ),
                ),
        );
        if let Some(p) = paths {
            g = g
                .child(row(
                    "Settings",
                    None,
                    mono(p.settings_file().display().to_string(), t),
                    t,
                ))
                .child(row(
                    "Themes",
                    None,
                    mono(p.themes().display().to_string(), t),
                    t,
                ))
                .child(row(
                    "Query spill files",
                    None,
                    mono(p.spill_dir().display().to_string(), t),
                    t,
                ));
        }
        g.child(row(
            "Drivers",
            None,
            mono(
                deltos_drivers::available()
                    .iter()
                    .map(|k| k.label())
                    .collect::<Vec<_>>()
                    .join(", "),
                t,
            ),
            t,
        ))
    }
}

/// The next value of `steps` above or below `v`; `0` in a list means "none" and sorts last.
fn step_up(v: u32, steps: &[u32]) -> u32 {
    let ix = steps.iter().position(|s| *s == v);
    match ix {
        Some(i) => steps.get(i + 1).copied().unwrap_or(v),
        None => steps
            .iter()
            .copied()
            .find(|s| *s > v || *s == 0)
            .unwrap_or(v),
    }
}

fn step_down(v: u32, steps: &[u32]) -> u32 {
    let ix = steps.iter().position(|s| *s == v);
    match ix {
        Some(0) => v,
        Some(i) => steps[i - 1],
        None => steps
            .iter()
            .copied()
            .rev()
            .find(|s| *s < v && *s != 0)
            .unwrap_or(steps[0]),
    }
}

fn group(title: &'static str, t: &Theme) -> Div {
    div().flex().flex_col().child(heading(title, t))
}

fn heading(title: &'static str, t: &Theme) -> Div {
    div()
        .pb(px(6.))
        .text_size(t.s(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text)
        .child(title)
}

fn gap() -> Div {
    div().h(px(22.))
}

fn mono(text: String, t: &Theme) -> AnyElement {
    div()
        .font_family(t.mono_font.clone())
        .text_size(t.small_size)
        .text_color(t.text_muted)
        .whitespace_normal()
        .child(text)
        .into_any_element()
}

/// A labelled setting: the name and an optional explanation on the left, the control on
/// the right, a hairline below.
fn row(label: &'static str, detail: Option<&'static str>, control: AnyElement, t: &Theme) -> Div {
    div()
        .min_h(t.s(40.))
        .py(px(7.))
        .flex()
        .items_center()
        .gap(px(16.))
        .border_b_1()
        .border_color(t.grid_line)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(div().text_size(t.ui_size).text_color(t.text).child(label))
                .children(detail.map(|d| {
                    div()
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child(d)
                })),
        )
        .child(div().flex_none().child(control))
}

/// A setting whose control is too wide for the right side: the name above, the control
/// below.
fn stacked_row(
    label: &'static str,
    detail: Option<&'static str>,
    control: AnyElement,
    t: &Theme,
) -> Div {
    div()
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .border_b_1()
        .border_color(t.grid_line)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(div().text_size(t.ui_size).text_color(t.text).child(label))
                .children(detail.map(|d| {
                    div()
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child(d)
                })),
        )
        .child(control)
}

fn toggle_row(
    label: &'static str,
    detail: Option<&'static str>,
    (id, on): (&'static str, bool),
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    t: &Theme,
) -> Div {
    row(
        label,
        detail,
        switch(id, on, t).on_click(on_click).into_any_element(),
        t,
    )
}

/// `−  value  +`.
fn stepper(
    id: &'static str,
    value: String,
    dec: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    inc: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    t: &Theme,
) -> AnyElement {
    let side = |suffix: &'static str, label: &'static str| {
        div()
            .id(ElementId::Name(format!("{id}-{suffix}").into()))
            .w(t.s(24.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(t.text_muted)
            .hover(|s| s.bg(t.hover).text_color(t.text))
            .child(label)
    };
    div()
        .h(t.s(26.))
        .flex()
        .items_center()
        .rounded(t.radius_small)
        .bg(t.input)
        .border_1()
        .border_color(t.border_strong)
        .overflow_hidden()
        .text_size(t.ui_size)
        .child(side("dec", "−").on_click(dec))
        .child(
            div()
                .min_w(px(72.))
                .px(px(6.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .border_l_1()
                .border_r_1()
                .border_color(t.border)
                .text_color(t.text)
                .child(value),
        )
        .child(side("inc", "+").on_click(inc))
        .into_any_element()
}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut nav = div()
            .w(px(190.))
            .flex_none()
            .h_full()
            .pt(px(12.))
            .px(px(8.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border);
        for (i, section) in Section::ALL.iter().copied().enumerate() {
            let selected = section == self.section;
            nav = nav.child(
                div()
                    .id(("section", i))
                    .h(t.list_row)
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(t.radius_small)
                    .cursor_pointer()
                    .text_size(t.ui_size)
                    .when(selected, |el| el.bg(t.selected).text_color(t.text))
                    .when(!selected, |el| {
                        el.text_color(t.text_muted).hover(|s| s.bg(t.hover))
                    })
                    .child(icon(section.icon()).size(px(13.)).text_color(if selected {
                        t.accent
                    } else {
                        t.text_faint
                    }))
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    })),
            );
        }
        let body = match self.section {
            Section::Appearance => self.appearance(&t, window, cx),
            Section::Data => self.data(&t, cx),
            Section::Editor => self.editor(&t, cx),
            Section::Updates => self.updates(&t, cx),
            Section::Keybindings => self.keybindings(&t, window, cx),
            Section::About => self.about(&t, cx),
        };
        div().size_full().flex().bg(t.surface).child(nav).child(
            div()
                .id("settings-body")
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .child(div().max_w(px(640.)).px(px(28.)).py(px(20.)).child(body)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_walk_the_list() {
        let steps = [50, 100, 250, 500];
        assert_eq!(step_up(100, &steps), 250);
        assert_eq!(step_up(500, &steps), 500);
        assert_eq!(step_down(50, &steps), 50);
        assert_eq!(step_down(300, &steps), 250);
        assert_eq!(step_up(300, &steps), 500);
        let with_none = [1024, 8192, 0];
        assert_eq!(step_up(8192, &with_none), 0);
        assert_eq!(step_down(0, &with_none), 8192);
    }
}
