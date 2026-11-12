//! Deltos: a fast, native database client.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod demo;
mod history_file;
mod lang;
mod net;
mod safety;
mod session;
mod snippets;
mod state;
mod theme;
mod ui;
mod update;

use std::borrow::Cow;
use std::rc::Rc;

use deltos_dock::{Rect, TreeId};
use deltos_settings::{PanelKind, Paths, Settings, WindowPlacement};
use gpui::{
    App, AppContext, Application, Bounds, Context, Entity, FontWeight, IntoElement, KeyBinding,
    Render, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    point, prelude::*, px, size,
};

use crate::net::Net;
use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::dock::DockView;
use crate::ui::icons::{Assets, Icon, icon};
use crate::ui::layouts::LayoutMenu;
use crate::ui::palette::PaletteLayer;
use crate::ui::palette::providers::Scope;
use crate::ui::workspace::Workspace;
use crate::ui::{Tooltip, dot, icon_button};
use crate::update::{UpdateModel, UpdateStatus};

const FONTS: [&[u8]; 12] = [
    include_bytes!("../../../assets/fonts/AdwaitaSans-Regular.ttf"),
    include_bytes!("../../../assets/fonts/AdwaitaSans-Medium.ttf"),
    include_bytes!("../../../assets/fonts/AdwaitaSans-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/AdwaitaSans-Bold.ttf"),
    include_bytes!("../../../assets/fonts/GoMono-Regular.ttf"),
    include_bytes!("../../../assets/fonts/GoMono-Bold.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
];

actions!(
    workspace,
    [
        NewQuery,
        CloseTab,
        NextTab,
        PreviousTab,
        Refresh,
        SearchObjects,
        ToggleSidebar,
        ToggleInspector,
        OpenSettings,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ResetLayout,
        CommandPalette,
    ]
);

/// Replaces every key binding: text fields and the editor first, then the shortcuts from
/// the settings. Unknown action names in the file are ignored.
pub fn rebind_keys(settings: &Settings, cx: &mut App) {
    cx.clear_key_bindings();
    ui::text_input::bind_keys(cx);
    ui::editor::bind_keys(cx);
    ui::grid::bind_keys(cx);
    ui::palette::bind_keys(cx);
    let ctx = Some("Workspace");
    let mut bindings = Vec::new();
    for (action, keys) in &settings.keybinds.0 {
        let keys = keys.as_str();
        if keys.is_empty() {
            continue;
        }
        let binding = match action.as_str() {
            "new-query" => KeyBinding::new(keys, NewQuery, ctx),
            "close-tab" => KeyBinding::new(keys, CloseTab, ctx),
            "next-tab" => KeyBinding::new(keys, NextTab, ctx),
            "previous-tab" => KeyBinding::new(keys, PreviousTab, ctx),
            "refresh" => KeyBinding::new(keys, Refresh, ctx),
            "search-objects" => KeyBinding::new(keys, SearchObjects, ctx),
            "toggle-sidebar" => KeyBinding::new(keys, ToggleSidebar, ctx),
            "toggle-inspector" => KeyBinding::new(keys, ToggleInspector, ctx),
            "settings" => KeyBinding::new(keys, OpenSettings, ctx),
            "zoom-in" => KeyBinding::new(keys, ZoomIn, ctx),
            "zoom-out" => KeyBinding::new(keys, ZoomOut, ctx),
            "zoom-reset" => KeyBinding::new(keys, ZoomReset, ctx),
            "reset-layout" => KeyBinding::new(keys, ResetLayout, ctx),
            "command-palette" | "command-palette-alt" => KeyBinding::new(keys, CommandPalette, ctx),
            "run-query" => KeyBinding::new(keys, ui::query::RunQuery, Some("QueryPanel")),
            // The inspector passes these on to the table its row came from.
            "commit-changes" => {
                bindings.push(KeyBinding::new(
                    keys,
                    ui::table::CommitChanges,
                    Some("Inspector"),
                ));
                KeyBinding::new(keys, ui::table::CommitChanges, Some("TablePanel"))
            }
            "review-changes" => {
                bindings.push(KeyBinding::new(
                    keys,
                    ui::table::ReviewChanges,
                    Some("Inspector"),
                ));
                KeyBinding::new(keys, ui::table::ReviewChanges, Some("TablePanel"))
            }
            "discard-changes" => {
                bindings.push(KeyBinding::new(
                    keys,
                    ui::table::DiscardChanges,
                    Some("Inspector"),
                ));
                KeyBinding::new(keys, ui::table::DiscardChanges, Some("TablePanel"))
            }
            "add-row" => KeyBinding::new(keys, ui::table::AddRow, Some("TablePanel")),
            // The grid's keys work on the grid itself, not in the filter fields beside it.
            "duplicate-rows" => KeyBinding::new(keys, ui::table::DuplicateRows, Some("Grid")),
            "paste-cells" => KeyBinding::new(keys, ui::grid::PasteCells, Some("Grid")),
            "jump-up" => KeyBinding::new(keys, ui::grid::JumpUp, Some("Grid")),
            "jump-down" => KeyBinding::new(keys, ui::grid::JumpDown, Some("Grid")),
            "jump-left" => KeyBinding::new(keys, ui::grid::JumpLeft, Some("Grid")),
            "jump-right" => KeyBinding::new(keys, ui::grid::JumpRight, Some("Grid")),
            // Each of several keys clears, as the Keyboard settings describe.
            "clear-cells" => {
                for key in keys.split_whitespace() {
                    bindings.push(KeyBinding::new(key, ui::grid::ClearCells, Some("Grid")));
                }
                continue;
            }
            // Tab also saves an open cell edit and moves on.
            "next-cell" => {
                let editor = Some(ui::grid::EDITOR_CONTEXT);
                bindings.push(KeyBinding::new(keys, ui::grid::NextCell, editor));
                KeyBinding::new(keys, ui::grid::NextCell, Some("Grid"))
            }
            "previous-cell" => {
                let editor = Some(ui::grid::EDITOR_CONTEXT);
                bindings.push(KeyBinding::new(keys, ui::grid::PreviousCell, editor));
                KeyBinding::new(keys, ui::grid::PreviousCell, Some("Grid"))
            }
            "run-all" => KeyBinding::new(keys, ui::query::RunAll, Some("QueryPanel")),
            "explain-query" => KeyBinding::new(keys, ui::query::ExplainQuery, Some("QueryPanel")),
            "format-sql" => KeyBinding::new(keys, ui::query::FormatSql, Some("QueryPanel")),
            "autocomplete" => KeyBinding::new(keys, ui::editor::ShowCompletions, Some("Editor")),
            "focus-filter" => {
                // In a table, wherever it is docked; elsewhere, the table in front.
                bindings.push(KeyBinding::new(
                    keys,
                    ui::table::FocusFilter,
                    Some("TablePanel"),
                ));
                KeyBinding::new(keys, ui::table::FocusFilter, ctx)
            }
            "save-query" => KeyBinding::new(keys, ui::query::SaveQuery, Some("QueryPanel")),
            _ => continue,
        };
        bindings.push(binding);
    }
    cx.bind_keys(bindings);
}

/// Deltos' own title bar asks the compositor for client-side decorations on Linux.
pub fn decorations(settings: &Settings) -> gpui::WindowDecorations {
    if settings.appearance.custom_title_bar && cfg!(target_os = "linux") {
        gpui::WindowDecorations::Client
    } else {
        gpui::WindowDecorations::Server
    }
}

/// The main window: the title bar and the main dock.
pub struct Root {
    state: Entity<AppState>,
    workspace: Entity<Workspace>,
    dock: Entity<DockView>,
    updates: Entity<UpdateModel>,
    palette: Entity<PaletteLayer>,
    layouts: Entity<LayoutMenu>,
    /// Whether the window was opened without the system title bar. Windows cannot drop or
    /// restore it on an open window, so the setting applies there from the next start.
    own_bar: bool,
    decorations: gpui::WindowDecorations,
    focus: gpui::FocusHandle,
    _subs: Vec<Subscription>,
}

impl Root {
    fn new(
        state: Entity<AppState>,
        workspace: Entity<Workspace>,
        updates: Entity<UpdateModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dock = cx.new(|cx| DockView::new(workspace.clone(), TreeId::Main, cx));
        let subs = vec![
            cx.subscribe_in(
                &state,
                window,
                |this, state, event, window, cx| match event {
                    StateEvent::Settings => {
                        let s = &state.read(cx).settings;
                        let own_bar = s.appearance.custom_title_bar;
                        // Windows cannot add or drop the system title bar on an open
                        // window, so the windows are opened again in the same place.
                        if cfg!(windows) && this.own_bar != own_bar {
                            this.own_bar = own_bar;
                            let old = window.window_handle();
                            let (state, workspace, updates) = (
                                this.state.clone(),
                                this.workspace.clone(),
                                this.updates.clone(),
                            );
                            cx.defer(move |cx| {
                                reopen_main_window(old, state, workspace, updates, cx)
                            });
                            return;
                        }
                        let wanted = decorations(s);
                        // Asking again makes some compositors redecorate the window, so only
                        // ask when the setting changed.
                        if this.decorations != wanted && !cfg!(windows) {
                            this.decorations = wanted;
                            window.request_decorations(wanted);
                        }
                        cx.notify();
                    }
                    StateEvent::Sessions | StateEvent::ActiveSession | StateEvent::Update => {
                        cx.notify()
                    }
                    _ => {}
                },
            ),
            cx.observe(&updates, |_, _, cx| cx.notify()),
            cx.observe(&workspace, |_, _, cx| cx.notify()),
            cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                let bounds = window.bounds();
                let maximized = window.is_maximized();
                this.workspace.update(cx, |w, cx| {
                    w.main_bounds = Some(bounds);
                    if !maximized {
                        w.layout.main_window = Some(WindowPlacement {
                            bounds: Rect {
                                x: f32::from(bounds.origin.x),
                                y: f32::from(bounds.origin.y),
                                w: f32::from(bounds.size.width),
                                h: f32::from(bounds.size.height),
                            },
                            maximized,
                        });
                    } else if let Some(p) = w.layout.main_window.as_mut() {
                        p.maximized = true;
                    }
                    w.save_layout(cx);
                });
            }),
            cx.observe_window_appearance(window, |this: &mut Self, _, cx| {
                this.state.update(cx, |s, cx| {
                    s.rebuild_theme(cx);
                    cx.refresh_windows();
                });
            }),
        ];
        let bounds = window.bounds();
        workspace.update(cx, |w, _| w.main_bounds = Some(bounds));
        let focus = cx.focus_handle();
        let palette =
            cx.new(|_| PaletteLayer::new(state.clone(), workspace.clone(), focus.clone()));
        let layouts = cx.new(|_| LayoutMenu::new(workspace.clone()));
        let s = &state.read(cx).settings;
        Self {
            own_bar: s.appearance.custom_title_bar,
            decorations: decorations(s),
            state,
            workspace,
            dock,
            updates,
            palette,
            layouts,
            focus,
            _subs: subs,
        }
    }

    /// The environment of the connection the window is about: the front tab's, or the
    /// active one.
    fn shown_environment(&self, cx: &App) -> Option<safety::Environment> {
        let id = ui::environment::shown_connection(self.workspace.read(cx), cx)?;
        safety::Environment::of(self.state.read(cx), &id)
    }

    /// The name, server version and environment of the connection the window is about,
    /// in a pill, as the title bar's middle.
    fn connection_pill(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = theme(cx).clone();
        let id = ui::environment::shown_connection(self.workspace.read(cx), cx);
        let s = self.state.read(cx);
        let shown = id
            .as_deref()
            .and_then(|id| Some((safety::Environment::of(s, id)?, s.session(id)?)));
        let Some((env, session)) = shown else {
            return div()
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child("Deltos")
                .into_any_element();
        };
        let tag = env.tag.color().map(theme::color);
        div()
            .h(t.s(22.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(t.radius)
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .child(dot(tag.unwrap_or(t.success), 7.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text)
                    .child(session.name.clone()),
            )
            .child(div().text_color(t.text_faint).child("·"))
            .child(
                div()
                    .text_color(t.text_muted)
                    .child(if session.version.is_empty() {
                        session.kind.label().to_owned()
                    } else {
                        session.version.clone()
                    }),
            )
            .children(ui::environment::badge(env.tag, &t))
            .children(ui::environment::lock(&env, &t))
            .into_any_element()
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = theme(cx).clone();
        let ws = self.workspace.read(cx);
        let toggle = |id: &'static str, which: Icon, on: bool, tip: &'static str| {
            icon_button(id, which, &t)
                .when(on, |el| el.text_color(t.accent))
                .tooltip(move |_, cx| Tooltip::view(tip, cx))
        };
        let (objects, history, inspector) = (
            ws.is_open(&PanelKind::Objects),
            ws.is_open(&PanelKind::History),
            ws.is_open(&PanelKind::Inspector),
        );
        let (w1, w2, w3, w4) = (
            self.workspace.clone(),
            self.workspace.clone(),
            self.workspace.clone(),
            self.workspace.clone(),
        );
        let status = self.updates.read(cx).status.clone();
        let updates = self.updates.clone();
        let pill = match status {
            UpdateStatus::Ready(v) => Some(
                div()
                    .id("update-pill")
                    .h(t.s(20.))
                    .px(px(9.))
                    .mr(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .rounded_full()
                    .bg(t.accent)
                    .text_color(t.accent_text)
                    .text_size(t.small_size)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.accent_hover))
                    .tooltip(move |_, cx| Tooltip::view(format!("Deltos {v} is ready"), cx))
                    .child(icon(Icon::Refresh).size(px(11.)))
                    .child("Restart to update")
                    .on_click(move |_, _, cx| updates.update(cx, |u, cx| u.restart_to_update(cx)))
                    .into_any_element(),
            ),
            UpdateStatus::Installing(_) => Some(
                div()
                    .mr(px(6.))
                    .text_size(t.small_size)
                    .text_color(t.text_muted)
                    .child("Updating…")
                    .into_any_element(),
            ),
            _ => None,
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .children(pill)
            .child(
                toggle("toggle-objects", Icon::SidebarLeft, objects, "Objects").on_click(
                    move |_, _, cx| w1.update(cx, |w, cx| w.toggle(PanelKind::Objects, cx)),
                ),
            )
            .child(
                toggle("toggle-history", Icon::PanelBottom, history, "History").on_click(
                    move |_, _, cx| w2.update(cx, |w, cx| w.toggle(PanelKind::History, cx)),
                ),
            )
            .child(
                toggle(
                    "toggle-inspector",
                    Icon::SidebarRight,
                    inspector,
                    "Inspector",
                )
                .on_click(move |_, _, cx| {
                    w3.update(cx, |w, cx| w.toggle(PanelKind::Inspector, cx))
                }),
            )
            .child(self.layouts.clone())
            .child(div().w(px(1.)).h(px(16.)).mx(px(4.)).bg(t.border))
            .child(
                icon_button("open-settings", Icon::Settings, &t)
                    .tooltip(|_, cx| Tooltip::view("Settings", cx))
                    .on_click(move |_, _, cx| {
                        w4.update(cx, |w, cx| w.open(PanelKind::Settings, true, cx))
                    }),
            )
            .into_any_element()
    }

    fn connections_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = theme(cx).clone();
        let ws = self.workspace.clone();
        div()
            .id("connections-button")
            .h(t.s(22.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(t.radius_small)
            .text_size(t.small_size)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover).text_color(t.text))
            .child(icon(Icon::Plug).size(px(13.)))
            .child("Connections")
            .on_click(move |_, _, cx| {
                ws.update(cx, |w, cx| w.open(PanelKind::Connections, true, cx))
            })
            .into_any_element()
    }

    fn zoom(&mut self, step: Option<f32>, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.update_settings(cx, |st| {
                let a = &mut st.appearance;
                a.ui_scale = match step {
                    Some(d) => ((a.ui_scale + d) * 20.).round() / 20.,
                    None => 1.0,
                }
                .clamp(0.75, 1.5);
            })
        });
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        // Until a Windows window is reopened it keeps the title bar it was opened with.
        let custom_bar = if cfg!(windows) {
            self.own_bar
        } else {
            self.state.read(cx).settings.appearance.custom_title_bar
        };
        let opacity = self.state.read(cx).settings.appearance.window_opacity;
        let bar = custom_bar.then(|| {
            let left = self.connections_menu(cx);
            let center = self.connection_pill(cx);
            let right = self.toolbar(cx);
            let ws = self.workspace.clone();
            ui::title_bar::title_bar(
                left,
                center,
                right,
                window,
                &t,
                Rc::new(move |window: &mut Window, cx: &mut App| {
                    quit_or_ask(&ws, window, cx);
                }),
            )
        });
        // Without our own bar the toolbar still needs a home: a slim strip at the top.
        let strip = (!custom_bar).then(|| {
            div()
                .flex_none()
                .h(t.s(32.))
                .px(px(8.))
                .flex()
                .items_center()
                .bg(t.title_bar)
                .border_b_1()
                .border_color(t.border)
                .when_some(t.title_bar_line, |el, line| {
                    el.border_b_2().border_color(line)
                })
                .child(self.connections_menu(cx))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .justify_center()
                        .child(self.connection_pill(cx)),
                )
                .child(self.toolbar(cx))
        });
        div()
            .id("root")
            .key_context("Workspace")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.base)
            .when(opacity < 0.999, |el| el.opacity(opacity))
            .text_color(t.text)
            .font_family(t.ui_font.clone())
            .text_size(t.ui_size)
            .on_action(cx.listener(|this, _: &NewQuery, _, cx| {
                let connection = this.state.read(cx).active.clone();
                this.workspace.update(cx, |w, cx| {
                    w.open(
                        PanelKind::Query {
                            connection,
                            text: String::new(),
                        },
                        true,
                        cx,
                    )
                });
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                this.workspace.update(cx, |w, cx| {
                    if let Some(p) = w.active_document() {
                        w.close(p, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| {
                this.workspace.update(cx, |w, cx| w.cycle_tab(1, cx))
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| {
                this.workspace.update(cx, |w, cx| w.cycle_tab(-1, cx))
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.workspace.update(cx, |w, cx| w.refresh_active(cx))
            }))
            .on_action(cx.listener(|this, _: &SearchObjects, window, cx| {
                this.palette
                    .update(cx, |p, cx| p.toggle(Scope::Objects, window, cx))
            }))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| {
                this.palette
                    .update(cx, |p, cx| p.toggle(Scope::All, window, cx))
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.workspace
                    .update(cx, |w, cx| w.toggle(PanelKind::Objects, cx))
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.workspace
                    .update(cx, |w, cx| w.toggle(PanelKind::Inspector, cx))
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.workspace
                    .update(cx, |w, cx| w.open(PanelKind::Settings, true, cx))
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(0.05), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-0.05), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_action(cx.listener(|this, _: &ResetLayout, _, cx| {
                this.workspace.update(cx, |w, cx| w.reset_layout(cx))
            }))
            .on_action(cx.listener(|this, _: &ui::table::FocusFilter, window, cx| {
                this.workspace.update(cx, |w, cx| {
                    if let Some(ui::workspace::PanelView::Table(t)) =
                        w.active_document().and_then(|p| w.view(p)).cloned()
                    {
                        t.update(cx, |t, cx| t.focus_filter(window, cx));
                    }
                })
            }))
            .children(bar)
            .children(strip)
            .children(ui::environment::strip(self.shown_environment(cx).as_ref()))
            // Focus is tracked below the bar, not on the whole window: a focusable element
            // under the title bar takes every press there, and on Windows a taken press
            // never reaches the system, so the window could not be dragged.
            .child(
                div()
                    .id("body")
                    .track_focus(&self.focus)
                    .flex_1()
                    .min_h_0()
                    .child(self.dock.clone()),
            )
            .when(custom_bar, |el| {
                el.child(ui::title_bar::resize_grips(window))
            })
            .child(self.palette.clone())
            .child(ui::transfer::overlay(cx))
    }
}

/// Quits, or first asks when tables hold staged changes that quitting would lose. `true`
/// when it quit.
fn quit_or_ask(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) -> bool {
    let n = workspace.read(cx).pending_changes(cx);
    if n == 0 {
        quit(workspace, window, cx);
        return true;
    }
    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        &format!(
            "Quit and discard {} staged {}?",
            ui::thousands(n as u64),
            if n == 1 { "change" } else { "changes" }
        ),
        Some("They have not been committed to the database. Commit them in their tables to keep them."),
        &["Discard and quit", "Cancel"],
        cx,
    );
    let (workspace, handle) = (workspace.clone(), window.window_handle());
    cx.spawn(async move |cx| {
        if answer.await.ok() == Some(0) {
            handle
                .update(cx, |_, window, cx| quit(&workspace, window, cx))
                .ok();
        }
    })
    .detach();
    false
}

/// Saves everything, installs a waiting update if the setting says so, and quits. The
/// panel windows close without docking so they open again next time.
fn quit(workspace: &Entity<Workspace>, _window: &mut Window, cx: &mut App) {
    session::begin_quit(cx);
    workspace.update(cx, |w, cx| {
        w.save_layout(cx);
        w.close_windows_for_quit(cx);
    });
    let state = workspace.read(cx).state.clone();
    state.update(cx, |s, cx| {
        s.save_now();
        // Ends each connection and its SSH tunnel.
        s.close_all(cx);
    });
    if let Some(u) = cx.try_global::<update::Updates>().map(|u| u.0.clone()) {
        u.update(cx, |u, cx| u.install_on_quit(cx));
    }
    cx.quit();
}

/// Opens the main window again over the old one, then closes the old one, so a change
/// of title bar takes effect at once on Windows. Panel windows are opened again too.
fn reopen_main_window(
    old: gpui::AnyWindowHandle,
    state: Entity<AppState>,
    workspace: Entity<Workspace>,
    updates: Entity<UpdateModel>,
    cx: &mut App,
) {
    workspace.update(cx, |w, cx| w.save_layout(cx));
    match open_main_window(state, workspace.clone(), updates, cx) {
        Ok(_) => {
            old.update(cx, |_, window, _| window.remove_window()).ok();
            workspace.update(cx, |w, cx| w.reopen_windows(cx));
        }
        Err(e) => tracing::error!("could not open the main window again: {e}"),
    }
}

fn open_main_window(
    state: Entity<AppState>,
    workspace: Entity<Workspace>,
    updates: Entity<UpdateModel>,
    cx: &mut App,
) -> anyhow::Result<gpui::WindowHandle<Root>> {
    let settings = &state.read(cx).settings;
    let custom_bar = settings.appearance.custom_title_bar;
    let placement = settings.layout.main_window;
    let bounds = match placement {
        Some(p) if p.maximized => WindowBounds::Maximized(Bounds {
            origin: point(px(p.bounds.x), px(p.bounds.y)),
            size: size(px(p.bounds.w), px(p.bounds.h)),
        }),
        Some(p) => WindowBounds::Windowed(Bounds {
            origin: point(px(p.bounds.x), px(p.bounds.y)),
            size: size(px(p.bounds.w), px(p.bounds.h)),
        }),
        None => WindowBounds::Windowed(Bounds::centered(None, size(px(1320.), px(820.)), cx)),
    };
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Deltos".into()),
            appears_transparent: custom_bar,
            ..Default::default()
        }),
        window_decorations: Some(decorations(settings)),
        window_bounds: Some(bounds),
        window_min_size: Some(size(px(720.), px(440.))),
        app_id: Some("deltos".into()),
        focus: true,
        show: true,
        ..Default::default()
    };
    let handle = cx.open_window(options, move |window, cx| {
        let quitting = workspace.clone();
        window.on_window_should_close(cx, move |window, cx| quit_or_ask(&quitting, window, cx));
        let root = cx.new(|cx| Root::new(state, workspace, updates, window, cx));
        window.focus(&root.read(cx).focus.clone());
        root
    })?;
    cx.activate(true);
    Ok(handle)
}

/// Writes the panic message and a backtrace to `path` before the process ends, so a crash
/// on a desktop without a terminal still leaves something to report.
fn install_crash_log(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let text = format!(
            "Deltos {} crashed at {secs} (seconds since 1970)\n{info}\n\n{backtrace}\n",
            env!("CARGO_PKG_VERSION"),
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, text);
        default(info);
    }));
}

fn load_settings(paths: Option<&Paths>) -> Settings {
    let Some(p) = paths else {
        return Settings::default();
    };
    let file = p.settings_file();
    tracing::info!("settings: {}", file.display());
    match Settings::load(&file) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("{e}; starting with defaults");
            Settings::default()
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DELTOS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("deltos=info,warn")),
        )
        .init();

    let paths = Paths::detect();
    if let Some(p) = &paths {
        install_crash_log(p.crash_log());
    }
    let demo = std::env::var("DELTOS_DEMO").ok().filter(|v| !v.is_empty());
    let mut settings = load_settings(paths.as_ref());
    if demo.is_some() {
        // The demo always starts from the default layout, so screenshots are stable.
        settings.layout = Default::default();
        settings.layout.main_window = None;
    } else if !settings.session.restore {
        settings.layout = settings.layout.without_documents();
    }
    let net = match Net::new() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("deltos: could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            cx.set_global(net);
            if let Err(e) = cx
                .text_system()
                .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
            {
                tracing::warn!("could not load the bundled fonts: {e}");
            }
            rebind_keys(&settings, cx);
            let recent = paths
                .as_ref()
                .filter(|_| demo.is_none())
                .map(|p| p.config_dir.join("palette-recent.txt"));
            ui::palette::recent::init(recent, cx);
            let state = state::init(settings, paths, demo.is_some(), cx);
            let updates = UpdateModel::init(state.clone(), cx);
            let workspace = cx.new(|cx| Workspace::new(state.clone(), cx));
            ui::layouts::init(&workspace, cx);
            cx.on_window_closed(|cx| {
                if !cx.windows().iter().any(|w| w.downcast::<Root>().is_some()) {
                    cx.quit();
                }
            })
            .detach();
            match open_main_window(state.clone(), workspace.clone(), updates, cx) {
                Ok(_) => {
                    workspace.update(cx, |w, cx| {
                        w.restore_windows(cx);
                        if let Some(screen) = demo.as_deref() {
                            w.start_demo(screen, cx);
                        }
                    });
                    session::init(&state, &workspace, cx);
                }
                Err(e) => {
                    eprintln!("deltos: could not open a window: {e}");
                    cx.quit();
                }
            }
        });
}
