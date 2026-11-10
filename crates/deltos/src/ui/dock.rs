//! Draws one window's dock tree: splits with draggable dividers, stacks with their tab
//! strip or header, the panels themselves, and the drop overlay while a panel is
//! dragged. Geometry is recorded into [`Workspace::zones`] as it paints, for hit tests.

use deltos_dock::{Axis, Node, PanelId, Stack, TreeId};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FontWeight, IntoElement, MouseButton, Pixels, Render,
    Subscription, Window, canvas, div, prelude::*, px, relative,
};

use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::workspace::{SplitResize, StackZone, TreeZones, Workspace};
use crate::ui::{Tooltip, empty_state, icon_button};

pub struct DockView {
    workspace: Entity<Workspace>,
    tree: TreeId,
    _subs: Vec<Subscription>,
}

impl DockView {
    pub fn new(workspace: Entity<Workspace>, tree: TreeId, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&workspace, |_, _, cx| cx.notify())];
        Self {
            workspace,
            tree,
            _subs: subs,
        }
    }

    /// Follows the pointer over the whole window while a drag or resize is on, even past
    /// the window's edge. See the GPUI notes in AGENTS.md for why a canvas does this.
    fn pointer_watch(&self) -> impl IntoElement {
        let ws = self.workspace.downgrade();
        let tree = self.tree;
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let moving = ws.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        moving
                            .update(cx, |w, cx| {
                                if w.drag.is_some() || w.resize.is_some() {
                                    w.pointer_moved(tree, e, window, cx);
                                }
                            })
                            .ok();
                    }
                });
                let releasing = ws.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture || e.button != MouseButton::Left {
                        return;
                    }
                    let consumed = releasing
                        .update(cx, |w, cx| w.pointer_released(tree, cx))
                        .unwrap_or(false);
                    if consumed {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }

    fn node(
        &self,
        node: &Node,
        path: Vec<usize>,
        ws: &Workspace,
        t: &Theme,
        cx: &App,
    ) -> AnyElement {
        match node {
            Node::Stack(s) => self.stack(s, ws, t, cx),
            Node::Split { axis, children } => {
                let row = *axis == Axis::Row;
                let tree = self.tree;
                let zones = ws.zones.clone();
                let record_path = path.clone();
                let mut el = div()
                    .relative()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .when(!row, |el| el.flex_col())
                    .child(
                        canvas(
                            move |b, _, _| {
                                zones
                                    .borrow_mut()
                                    .entry(tree)
                                    .or_default()
                                    .splits
                                    .insert(record_path, b);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    );
                let sizes: Vec<f32> = children.iter().map(|c| c.size).collect();
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        el = el.child(
                            div()
                                .flex_none()
                                .bg(t.border)
                                .when(row, |el| el.w(px(1.)).h_full())
                                .when(!row, |el| el.h(px(1.)).w_full()),
                        );
                    }
                    let mut p = path.clone();
                    p.push(i);
                    let mut cell = div()
                        .relative()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .flex_basis(relative(0.))
                        .when(row, |el| el.h_full())
                        .when(!row, |el| el.w_full())
                        .child(self.node(&child.node, p, ws, t, cx));
                    cell.style().flex_grow = Some(child.size.max(0.0001) * 1000.0);
                    cell.style().flex_shrink = Some(1.0);
                    el = el.child(cell);
                }
                // The dividers' grab areas go on top of the panels, so a panel's own
                // handlers never take the press.
                let mut cumulative = 0.0;
                for i in 0..children.len().saturating_sub(1) {
                    cumulative += sizes[i];
                    let handle_path = path.clone();
                    let start_sizes = sizes.clone();
                    let active = ws
                        .resize
                        .as_ref()
                        .is_some_and(|r| r.tree == tree && r.path == path && r.handle == i);
                    let zones = ws.zones.clone();
                    el = el.child(
                        div()
                            .id(("divider", i))
                            .absolute()
                            .when(row, |el| {
                                el.top_0()
                                    .bottom_0()
                                    .left(relative(cumulative))
                                    .ml(px(-3.))
                                    .w(px(7.))
                                    .cursor_col_resize()
                            })
                            .when(!row, |el| {
                                el.left_0()
                                    .right_0()
                                    .top(relative(cumulative))
                                    .mt(px(-3.))
                                    .h(px(7.))
                                    .cursor_row_resize()
                            })
                            .child(
                                div()
                                    .absolute()
                                    .when(row, |el| el.top_0().bottom_0().left(px(2.)).w(px(3.)))
                                    .when(!row, |el| el.left_0().right_0().top(px(2.)).h(px(3.)))
                                    .when(active, |el| el.bg(t.accent))
                                    .when(!active, |el| el.hover(|s| s.bg(t.accent_soft))),
                            )
                            .on_mouse_down(MouseButton::Left, {
                                let workspace = self.workspace.clone();
                                move |e: &gpui::MouseDownEvent, _: &mut Window, cx: &mut App| {
                                    cx.stop_propagation();
                                    let length = zones
                                        .borrow()
                                        .get(&tree)
                                        .and_then(|z| z.splits.get(&handle_path).copied())
                                        .map_or(800.0, |b: Bounds<Pixels>| {
                                            f32::from(if row {
                                                b.size.width
                                            } else {
                                                b.size.height
                                            })
                                        });
                                    let resize = SplitResize {
                                        tree,
                                        path: handle_path.clone(),
                                        handle: i,
                                        row,
                                        start: e.position,
                                        length,
                                        start_sizes: start_sizes.clone(),
                                    };
                                    workspace.update(cx, |w, cx| w.begin_resize(resize, cx));
                                }
                            }),
                    );
                }
                el.into_any_element()
            }
        }
    }

    fn stack(&self, s: &Stack, ws: &Workspace, t: &Theme, cx: &App) -> AnyElement {
        let tree = self.tree;
        let Some(active) = s.active_panel() else {
            return div().into_any_element();
        };
        let active_view = ws.view(active).cloned();
        let tool_header = s.panels.len() == 1 && active_view.as_ref().is_some_and(|v| v.is_tool());
        let sidebar_bg = active_view.as_ref().is_some_and(|v| v.is_sidebar());
        let dragging = ws.drag.as_ref().filter(|d| d.active).map(|d| d.panel);
        let anchor = s.panels[0];
        let panels = s.panels.clone();
        let zones = ws.zones.clone();
        let strip_bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<Pixels>>));
        let content_bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<Pixels>>));

        let header = if tool_header {
            self.tool_header(active, ws, t, cx)
        } else {
            self.tab_strip(s, ws, t, dragging, cx)
        };
        let record_strip = strip_bounds.clone();
        let record_content = content_bounds.clone();
        let record = {
            let (strip, content) = (strip_bounds.clone(), content_bounds.clone());
            move || {
                if let (Some(strip), Some(content)) = (strip.get(), content.get()) {
                    zones
                        .borrow_mut()
                        .entry(tree)
                        .or_default()
                        .stacks
                        .push(StackZone {
                            anchor,
                            panels: panels.clone(),
                            strip,
                            content,
                        });
                }
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(if sidebar_bg { t.sidebar } else { t.surface })
            .child(
                div().relative().flex_none().child(header).child(
                    canvas(move |b, _, _| record_strip.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .inset_0(),
                ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .children(active_view.map(|v| v.view()))
                    .child(
                        canvas(
                            move |b, _, _| {
                                record_content.set(Some(b));
                                record();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
            .into_any_element()
    }

    fn press_handler(
        &self,
        panel: PanelId,
    ) -> impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
        let tree = self.tree;
        let ws = self.workspace.clone();
        move |e: &gpui::MouseDownEvent, _, cx| {
            if e.click_count >= 2 {
                ws.update(cx, |w, cx| w.pin(panel, cx));
                return;
            }
            ws.update(cx, |w, cx| w.press(panel, tree, e.position, cx));
        }
    }

    fn tool_header(&self, panel: PanelId, ws: &Workspace, t: &Theme, cx: &App) -> AnyElement {
        let title = ws.view(panel).map(|v| v.title(cx)).unwrap_or_default();
        let floating = matches!(self.tree, TreeId::Floating(_));
        let press = self.press_handler(panel);
        let close_ws = self.workspace.clone();
        let pop_ws = self.workspace.clone();
        div()
            .id(("tool-header", panel.0))
            .h(t.tab_height)
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(t.sidebar)
            .border_b_1()
            .border_color(t.border)
            .on_mouse_down(MouseButton::Left, press)
            .child(
                div()
                    .flex_1()
                    .text_size(t.s(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_muted)
                    .child(title.to_uppercase()),
            )
            .when(!floating, |el| {
                el.child(
                    icon_button(("pop", panel.0), Icon::PopOut, t)
                        .tooltip(|_, cx| Tooltip::view("Open in a new window", cx))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, _, cx| {
                            pop_ws.update(cx, |w, cx| w.pop_out(panel, None, false, cx));
                        }),
                )
            })
            .child(
                icon_button(("close", panel.0), Icon::Close, t)
                    .tooltip(|_, cx| Tooltip::view("Close", cx))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| close_ws.update(cx, |w, cx| w.close(panel, cx))),
            )
            .into_any_element()
    }

    fn tab_strip(
        &self,
        s: &Stack,
        ws: &Workspace,
        t: &Theme,
        dragging: Option<PanelId>,
        cx: &App,
    ) -> AnyElement {
        let tree = self.tree;
        let active = s.active_panel();
        let mut strip = div()
            .id(("strip", s.panels[0].0))
            .h(t.tab_height)
            .w_full()
            .flex()
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .overflow_x_scroll();
        for &panel in &s.panels {
            let Some(view) = ws.view(panel) else { continue };
            let is_active = active == Some(panel);
            let preview = ws.is_preview(panel);
            let title = view.title(cx);
            let which = view.icon(cx);
            let press = self.press_handler(panel);
            let close_ws = self.workspace.clone();
            let zones = ws.zones.clone();
            let lifted = dragging == Some(panel);
            strip = strip.child(
                div()
                    .id(("tab", panel.0))
                    .group("tab")
                    .relative()
                    .flex_none()
                    .min_w(t.s(96.))
                    .max_w(t.s(220.))
                    .h_full()
                    .pl(px(10.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_r_1()
                    .border_color(t.border)
                    .text_size(t.ui_size)
                    .when(is_active, |el| {
                        el.bg(t.surface).text_color(t.text).child(
                            // The active tab joins the panel below it.
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom(px(-1.))
                                .h(px(1.))
                                .bg(t.surface),
                        )
                    })
                    .when(!is_active, |el| {
                        el.text_color(t.text_muted)
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                    })
                    .when(lifted, |el| el.opacity(0.5))
                    .on_mouse_down(MouseButton::Left, press)
                    .child(icon(which).size(t.s(13.)).text_color(if is_active {
                        t.accent
                    } else {
                        t.text_faint
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(preview, |el| el.italic())
                            .child(title),
                    )
                    .child(
                        div()
                            .id(("tab-close", panel.0))
                            .flex_none()
                            .size(t.s(18.))
                            .rounded(t.radius_small)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(t.text_faint)
                            .when(!is_active, |el| {
                                el.opacity(0.).group_hover("tab", |s| s.opacity(1.))
                            })
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(move |_, _, cx| {
                                close_ws.update(cx, |w, cx| w.close(panel, cx))
                            })
                            .child(icon(Icon::Close).size(t.s(11.))),
                    )
                    .child(
                        canvas(
                            move |b, _, _| {
                                zones
                                    .borrow_mut()
                                    .entry(tree)
                                    .or_default()
                                    .tabs
                                    .insert(panel, b);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
            );
        }
        let floating = matches!(tree, TreeId::Floating(_));
        let pop_ws = self.workspace.clone();
        let new_ws = self.workspace.clone();
        let state = ws.state.clone();
        strip = strip.child(div().flex_1().h_full()).child(
            div()
                .flex_none()
                .h_full()
                .px(px(4.))
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button(("new-query", s.panels[0].0), Icon::Plus, t)
                        .tooltip(|_, cx| Tooltip::view("New query", cx))
                        .on_click(move |_, _, cx| {
                            let connection = state.read(cx).active.clone();
                            new_ws.update(cx, |w, cx| {
                                w.open(
                                    deltos_settings::PanelKind::Query {
                                        connection,
                                        text: String::new(),
                                    },
                                    true,
                                    cx,
                                )
                            });
                        }),
                )
                .when_some(active.filter(|_| !floating), |el, panel| {
                    el.child(
                        icon_button(("pop", panel.0), Icon::PopOut, t)
                            .tooltip(|_, cx| Tooltip::view("Open in a new window", cx))
                            .on_click(move |_, _, cx| {
                                pop_ws.update(cx, |w, cx| w.pop_out(panel, None, false, cx));
                            }),
                    )
                }),
        );
        strip.into_any_element()
    }
}

impl Render for DockView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let ws_entity = self.workspace.clone();
        let ws = ws_entity.read(cx);
        let tree_id = self.tree;
        // Geometry is recorded afresh by this paint.
        ws.zones.borrow_mut().insert(tree_id, TreeZones::default());
        let root = ws.layout.dock.tree(tree_id).and_then(|t| t.root.clone());
        let body = match &root {
            Some(node) => self.node(node, Vec::new(), ws, &t, cx),
            None => empty_state(
                "Nothing here",
                Some("Drag a panel here, or open a table from the sidebar.".into()),
                &t,
            )
            .bg(t.surface)
            .into_any_element(),
        };
        let ws = ws_entity.read(cx);
        let drag = ws.drag.clone().filter(|d| d.active);
        let hover = drag
            .as_ref()
            .and_then(|d| d.hover.clone())
            .or_else(|| ws.redock.as_ref().map(|r| r.1.clone()))
            .filter(|h| h.tree == tree_id);
        let ghost = drag.as_ref().filter(|d| d.source == tree_id).map(|d| {
            (
                d.cursor,
                ws.view(d.panel).map(|v| v.title(cx)).unwrap_or_default(),
            )
        });
        let resizing = ws
            .resize
            .as_ref()
            .filter(|r| r.tree == tree_id)
            .map(|r| r.row);
        let zones = ws.zones.clone();
        div()
            .id("dock")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(t.base)
            .child(body)
            .child(
                canvas(
                    move |b, _, _| {
                        zones.borrow_mut().entry(tree_id).or_default().root = Some(b);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(self.pointer_watch())
            .when_some(resizing, |el, row| {
                if row {
                    el.cursor_col_resize()
                } else {
                    el.cursor_row_resize()
                }
            })
            .when(drag.is_some(), |el| el.cursor_grabbing())
            .children(hover.map(|h| {
                let b = h.highlight;
                div()
                    .absolute()
                    .left(b.left())
                    .top(b.top())
                    .w(b.size.width)
                    .h(b.size.height)
                    .rounded(t.radius_small)
                    .when(h.caret, |el| el.bg(t.accent))
                    .when(!h.caret, |el| {
                        el.bg(t.accent_soft).border_2().border_color(t.accent)
                    })
            }))
            .children(ghost.map(|(at, title)| {
                div()
                    .absolute()
                    .left(at.x + px(12.))
                    .top(at.y + px(10.))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(t.radius_small)
                    .bg(t.floating)
                    .border_1()
                    .border_color(t.border_strong)
                    .shadow_md()
                    .text_size(t.small_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text)
                    .child(title)
            }))
    }
}
