//! Settings, grouped into sections down the left. Every change applies at once and is
//! written to `settings.toml` a moment later.

use deltos_settings::{DateFormat, Density, KEYBIND_ACTIONS, NullStyle, Settings, ThemeMode};
use gpui::{
    AnyElement, App, AppContext, Context, Div, ElementId, Entity, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::state::{AppState, StateEvent};
use crate::theme::{Theme, color, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, button, field, menu, segmented, switch};
use crate::update::{CURRENT, UpdateStatus, Updates};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Data,
    Editor,
    Updates,
    Keybindings,
    About,
}

impl Section {
    const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Data,
        Self::Editor,
        Self::Updates,
        Self::Keybindings,
        Self::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Data => "Data",
            Self::Editor => "Editor",
            Self::Updates => "Updates",
            Self::Keybindings => "Keyboard",
            Self::About => "About",
        }
    }

    fn icon(self) -> Icon {
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
const ACCENTS: [u32; 8] = [
    0x2F7CF6, 0x7C5CF6, 0xD9467A, 0xE5484D, 0xEE7D2B, 0x2E9F5E, 0x12A0A0, 0x6E6E73,
];

pub struct SettingsPanel {
    state: Entity<AppState>,
    section: Section,
    ui_font: Entity<TextInput>,
    mono_font: Entity<TextInput>,
    editor_font: Entity<TextInput>,
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
        let ui_font = text_input("Inter", s.appearance.ui_font.clone(), cx);
        let mono_font = text_input("JetBrains Mono", s.appearance.mono_font.clone(), cx);
        let editor_font = text_input("Same as the grid", s.editor.font.clone(), cx);
        let keys: Vec<_> = KEYBIND_ACTIONS
            .iter()
            .map(|(action, default, _)| {
                (
                    *action,
                    text_input(default, s.keybinds.get(action).map(str::to_owned), cx),
                )
            })
            .collect();
        let mut subs = vec![cx.subscribe(&state, |_, _, e, cx| {
            if matches!(e, StateEvent::Settings | StateEvent::Update) {
                cx.notify();
            }
        })];
        let font_setter = |which: fn(&mut Settings, Option<String>)| {
            move |this: &mut Self,
                  input: Entity<TextInput>,
                  e: &InputEvent,
                  cx: &mut Context<Self>| {
                if let InputEvent::Submit = e {
                    let text = input.read(cx).text().trim().to_owned();
                    let value = (!text.is_empty()).then_some(text);
                    this.set(cx, move |s| which(s, value));
                }
            }
        };
        subs.push(cx.subscribe(&ui_font, font_setter(|s, v| s.appearance.ui_font = v)));
        subs.push(cx.subscribe(&mono_font, font_setter(|s, v| s.appearance.mono_font = v)));
        subs.push(cx.subscribe(&editor_font, font_setter(|s, v| s.editor.font = v)));
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
            ui_font,
            mono_font,
            editor_font,
            keys,
            theme_menu: false,
            _subs: subs,
        }
    }

    fn set(&self, cx: &mut App, f: impl FnOnce(&mut Settings)) {
        self.state.update(cx, |s, cx| s.update_settings(cx, f));
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
        let mut swatches = div().flex().gap(px(6.));
        for (i, c) in ACCENTS.iter().copied().enumerate() {
            let selected = a.accent == c;
            let w = cx.entity().downgrade();
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
                        w.update(cx, |this, cx| this.set(cx, |s| s.appearance.accent = c))
                            .ok();
                    }),
            );
        }
        let s1 = state.clone();
        let s2 = state.clone();
        let s3 = state.clone();
        let s4 = state.clone();
        group("Theme", t)
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
            .child(row(
                "Theme file",
                Some("Colors from a .toml file in the themes folder, over the light or dark base."),
                theme_control.into_any_element(),
                t,
            ))
            .child(row("Accent", None, swatches.into_any_element(), t))
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
            .child(row(
                "Interface font",
                Some("Empty for Inter."),
                font_field(&self.ui_font, t, window, cx),
                t,
            ))
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
                Some("Empty for JetBrains Mono."),
                font_field(&self.mono_font, t, window, cx),
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

    fn editor(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let e = self.state.read(cx).settings.editor.clone();
        let w = cx.entity().downgrade();
        let pick = move |f: fn(&mut Settings)| {
            let w = w.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                w.update(cx, |this, cx| this.set(cx, f)).ok();
            }
        };
        group("Editor", t)
            .child(row(
                "Font",
                Some("Empty for the grid font."),
                font_field(&self.editor_font, t, window, cx),
                t,
            ))
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

fn font_field(input: &Entity<TextInput>, t: &Theme, window: &Window, cx: &App) -> AnyElement {
    div()
        .w(px(200.))
        .child(field(input, t, window, cx))
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
            Section::Editor => self.editor(&t, window, cx),
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
