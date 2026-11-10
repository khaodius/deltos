//! Deltos' own title bar, for when the system draws none (GNOME on Wayland) or the person
//! prefers the slimmer look: the mark on the left, whatever the window puts in the
//! middle and on the right, then minimize, maximize and close. On Windows the buttons and
//! the drag area are handed to the system, which keeps snap layouts and double-click to
//! maximize; on Linux Deltos asks the compositor to move, resize and maximize.

use std::rc::Rc;

use gpui::{
    AnyElement, App, CursorStyle, Decorations, IntoElement, MouseButton, Pixels, Point, ResizeEdge,
    Size, Window, WindowControlArea, canvas, div, prelude::*, px,
};

use crate::theme::Theme;
use crate::ui::icons::{Icon, icon};

pub const HEIGHT: f32 = 34.;
/// How far in from the window's edge a press starts a resize.
const GRIP: f32 = 6.;

pub type OnClose = Rc<dyn Fn(&mut Window, &mut App)>;

/// The window buttons, for any window that draws its own bar.
pub fn window_buttons(window: &Window, t: &Theme, on_close: OnClose) -> AnyElement {
    let maximized = window.is_maximized();
    let button = |id: &'static str, which: Icon, area: WindowControlArea, danger: bool| {
        div()
            .id(id)
            .flex_none()
            .w(px(44.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(t.text_muted)
            .when(danger, |el| {
                el.hover(|s| s.bg(gpui::rgb(0xe81123)).text_color(gpui::white()))
            })
            .when(!danger, |el| el.hover(|s| s.bg(t.hover).text_color(t.text)))
            .when(cfg!(windows), |el| el.window_control_area(area))
            .child(icon(which).size(px(14.)))
    };
    div()
        .flex()
        .flex_none()
        .h_full()
        .child(
            button("win-min", Icon::Minimize, WindowControlArea::Min, false)
                .on_click(|_, window, _| window.minimize_window()),
        )
        .child(
            button(
                "win-max",
                if maximized {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                WindowControlArea::Max,
                false,
            )
            .on_click(|_, window, _| window.zoom_window()),
        )
        .child(
            button("win-close", Icon::Close, WindowControlArea::Close, true)
                .on_click(move |_, window, cx| on_close(window, cx)),
        )
        .into_any_element()
}

/// Makes an element move the window when dragged, and maximize on double click.
pub fn drag_area<E>(el: E) -> E
where
    E: InteractiveElement + StatefulInteractiveElement + Styled,
{
    if cfg!(windows) {
        el.window_control_area(WindowControlArea::Drag)
    } else {
        el.on_mouse_down(MouseButton::Left, |e, window, _| {
            if e.click_count >= 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        })
        .on_mouse_down(MouseButton::Right, |e, window, _| {
            window.show_window_menu(e.position);
        })
    }
}

/// The main window's bar: the mark, `left` beside it, `center` in the middle of the
/// window, `right` before the window buttons. Everything that is not a control drags.
pub fn title_bar(
    left: AnyElement,
    center: AnyElement,
    right: AnyElement,
    window: &Window,
    t: &Theme,
    on_close: OnClose,
) -> AnyElement {
    div()
        .id("title-bar")
        .flex_none()
        .h(t.s(HEIGHT))
        .w_full()
        .relative()
        .flex()
        .items_center()
        .bg(t.title_bar)
        .border_b_1()
        .border_color(t.border)
        .child(
            // The drag area sits under everything, so the bar moves the window wherever
            // nothing else takes the press.
            drag_area(div().id("title-drag").absolute().inset_0()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .pl(px(12.))
                .child(icon(Icon::Logo).size(px(15.)).text_color(t.accent))
                .child(left),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(220.))
                .right(px(220.))
                .flex()
                .items_center()
                .justify_center()
                .child(center),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .pr(px(8.))
                .child(right),
        )
        .child(window_buttons(window, t, on_close))
        .into_any_element()
}

/// On Linux with Deltos' own decorations, the strip just inside each edge resizes the
/// window, as the system's frame would.
pub fn resize_grips(window: &Window) -> impl IntoElement {
    let tiling = match window.window_decorations() {
        Decorations::Client { tiling } if !cfg!(windows) => Some(tiling),
        _ => None,
    };
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let Some(tiling) = tiling else { return };
            if window.is_maximized() || tiling.is_tiled() {
                return;
            }
            let size = window.viewport_size();
            if let Some(edge) = edge_at(window.mouse_position(), size) {
                window.set_window_cursor_style(match edge {
                    ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                    ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                        CursorStyle::ResizeUpLeftDownRight
                    }
                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                        CursorStyle::ResizeUpRightDownLeft
                    }
                });
            }
            window.on_mouse_event(|e: &gpui::MouseMoveEvent, phase, window, _| {
                if phase != gpui::DispatchPhase::Capture || e.pressed_button.is_some() {
                    return;
                }
                // Redraw only when the pointer moves onto another edge or off the edges,
                // not on every move along one.
                let edge = edge_at(e.position, window.viewport_size());
                if HOVERED_EDGE.replace(edge) != edge {
                    window.refresh();
                }
            });
            window.on_mouse_event(|e: &gpui::MouseDownEvent, phase, window, cx| {
                if phase != gpui::DispatchPhase::Capture || e.button != MouseButton::Left {
                    return;
                }
                if let Some(edge) = edge_at(e.position, window.viewport_size()) {
                    window.start_window_resize(edge);
                    cx.stop_propagation();
                }
            });
        },
    )
    .absolute()
    .size_0()
}

thread_local! {
    static HOVERED_EDGE: std::cell::Cell<Option<ResizeEdge>> = const { std::cell::Cell::new(None) };
}

fn edge_at(p: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let g = px(GRIP);
    let corner = px(GRIP * 2.);
    let left = p.x < g;
    let right = p.x > size.width - g;
    let top = p.y < g;
    let bottom = p.y > size.height - g;
    let near_left = p.x < corner;
    let near_right = p.x > size.width - corner;
    let near_top = p.y < corner;
    let near_bottom = p.y > size.height - corner;
    Some(match () {
        _ if (top && near_left) || (left && near_top) => ResizeEdge::TopLeft,
        _ if (top && near_right) || (right && near_top) => ResizeEdge::TopRight,
        _ if (bottom && near_left) || (left && near_bottom) => ResizeEdge::BottomLeft,
        _ if (bottom && near_right) || (right && near_bottom) => ResizeEdge::BottomRight,
        _ if top => ResizeEdge::Top,
        _ if bottom => ResizeEdge::Bottom,
        _ if left => ResizeEdge::Left,
        _ if right => ResizeEdge::Right,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges() {
        let s = gpui::size(px(800.), px(600.));
        assert_eq!(
            edge_at(gpui::point(px(2.), px(300.)), s),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            edge_at(gpui::point(px(2.), px(2.)), s),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            edge_at(gpui::point(px(799.), px(599.)), s),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(edge_at(gpui::point(px(400.), px(300.)), s), None);
    }
}
