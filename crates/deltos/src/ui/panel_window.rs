//! A window of panels torn off the main one. It has its own dock tree, so panels can be
//! split and tabbed in it too. Its header moves it; moved over the main window it
//! offers to dock there. Its Dock button, or closing it, puts its panels back.

use std::time::Instant;

use deltos_dock::{TreeId, WindowId};
use gpui::{
    Context, Entity, FontWeight, IntoElement, MouseButton, Render, Subscription, Window, div,
    prelude::*, px,
};

use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::dock::DockView;
use crate::ui::icons::{Icon, icon};
use crate::ui::title_bar::{self, window_buttons};
use crate::ui::workspace::Workspace;
use crate::ui::{ButtonStyle, Tooltip, button};

pub struct PanelWindow {
    workspace: Entity<Workspace>,
    id: WindowId,
    dock: Entity<DockView>,
    opened: Instant,
    _subs: Vec<Subscription>,
}

impl PanelWindow {
    pub fn new(
        workspace: Entity<Workspace>,
        state: Entity<AppState>,
        id: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dock = cx.new(|cx| DockView::new(workspace.clone(), TreeId::Floating(id), cx));
        let closing = workspace.clone();
        window.on_window_should_close(cx, move |_, cx| {
            closing.update(cx, |w, cx| w.window_closing(id, cx));
            true
        });
        let appearance_state = state.clone();
        // The workspace is mid-update while this window opens, so nothing here reads it.
        let subs = vec![
            cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                let (bounds, pointer, opened, id) = (
                    window.bounds(),
                    window.mouse_position(),
                    this.opened,
                    this.id,
                );
                this.workspace.update(cx, |w, cx| {
                    w.window_bounds_changed(id, bounds, pointer, opened, cx)
                });
            }),
            cx.observe(&workspace, |this: &mut Self, ws, cx| {
                let _ = (this, ws);
                cx.notify();
            }),
            cx.subscribe_in(&state, window, |_, _, event, window, cx| {
                if let StateEvent::Settings = event {
                    window.refresh();
                    cx.notify();
                }
            }),
            cx.observe_window_appearance(window, move |_: &mut Self, window, cx| {
                follow_appearance(&appearance_state, window.appearance(), cx);
            }),
        ];
        Self {
            workspace,
            id,
            dock,
            opened: Instant::now(),
            _subs: subs,
        }
    }
}

impl Render for PanelWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let id = self.id;
        let title = self.workspace.read(cx).window_title(id, cx);
        window.set_window_title(&format!("{title} - Deltos"));
        let own_bar = self
            .workspace
            .read(cx)
            .state
            .read(cx)
            .settings
            .appearance
            .custom_title_bar;
        let press_ws = self.workspace.clone();
        let dock_ws = self.workspace.clone();
        let close_ws = self.workspace.clone();
        let header = div()
            .flex_none()
            .h(t.s(32.))
            .flex()
            .items_center()
            .bg(t.title_bar)
            .border_b_1()
            .border_color(t.border)
            .when_some(t.title_bar_line, |el, line| {
                el.border_b_2().border_color(line)
            })
            .child(
                // The header moves the window, as it may have no system title bar. The
                // press is recorded first so the move can be matched to the main window.
                div()
                    .id("panel-drag")
                    .flex_1()
                    .h_full()
                    .pl(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .when(cfg!(windows), |el| {
                        el.window_control_area(gpui::WindowControlArea::Drag)
                    })
                    .when(!cfg!(windows), |el| {
                        el.on_mouse_down(MouseButton::Left, move |e, window, cx| {
                            press_ws.update(cx, |w, _| w.begin_window_move(id, e.position, window));
                            window.start_window_move();
                        })
                    })
                    .child(icon(Icon::Logo).size(px(13.)).text_color(t.accent))
                    .child(
                        div()
                            .text_size(t.small_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.text_muted)
                            .child(title),
                    ),
            )
            .child(
                div().px(px(6.)).child(
                    button("dock", "Dock", ButtonStyle::Ghost, &t)
                        .h(t.s(22.))
                        .px(px(8.))
                        .tooltip(|_, cx| Tooltip::view("Put back in the main window", cx))
                        .on_click(move |_, window, cx| {
                            dock_ws.update(cx, |w, cx| w.dock_window(id, cx));
                            window.remove_window();
                        }),
                ),
            )
            .when(own_bar, |el| {
                el.child(window_buttons(
                    window,
                    &t,
                    std::rc::Rc::new(move |window: &mut Window, cx: &mut gpui::App| {
                        close_ws.update(cx, |w, cx| w.window_closing(id, cx));
                        window.remove_window();
                    }),
                ))
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.base)
            .text_color(t.text)
            .font_family(t.ui_font.clone())
            .text_size(t.ui_size)
            // Without system decorations a hairline keeps the window apart from what is
            // behind it, as small panel windows often sit over the main one.
            .when(own_bar, |el| el.border_1().border_color(t.border_strong))
            .child(header)
            .child(
                // A torn-off panel lands in its new window with the same squash and
                // stretch as one docking.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(crate::ui::dock::settle(
                        div().absolute().inset_0().child(self.dock.clone()),
                        t.motion.then_some(("intro", 0)),
                        &t,
                    )),
            )
            .when(own_bar, |el| el.child(title_bar::resize_grips(window)))
            .child(crate::ui::transfer::overlay(cx))
    }
}

/// Takes the system appearance a window reported. gpui on Wayland calls the appearance
/// observers while it holds its own state, and any call back into the platform from there
/// panics ("RefCell already borrowed"), so the theme is rebuilt on the next turn of the
/// event loop, from the appearance the window passed in.
pub fn follow_appearance<T: 'static>(
    state: &Entity<AppState>,
    appearance: gpui::WindowAppearance,
    cx: &mut Context<T>,
) {
    let state = state.clone();
    cx.spawn(async move |_, cx| {
        state
            .update(cx, |s, cx| s.set_system_appearance(appearance, cx))
            .ok();
    })
    .detach();
}
