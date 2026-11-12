//! A horizontal slider: a track, its filled part and a knob. Press anywhere on the track
//! to jump there, and drag to move; the drag follows the pointer past the track.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ElementId, MouseButton, Pixels, Window, canvas, div, prelude::*, px,
    relative,
};

use crate::theme::Theme;

#[derive(Default)]
struct Drag {
    on: bool,
    track: Bounds<Pixels>,
    /// The last value sent, so a drag that does not move the value sends nothing.
    last: Option<f32>,
}

/// A slider from `min` to `max` in steps of `step`, showing `value` and `label` beside it.
/// `on_change` gets each new value as the knob moves.
pub fn slider(
    id: &'static str,
    value: f32,
    (min, max, step): (f32, f32, f32),
    label: String,
    on_change: impl Fn(f32, &mut App) + 'static,
    t: &Theme,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = window.use_keyed_state(
        ElementId::Name(format!("slider-{id}").into()),
        cx,
        |_, _| Drag::default(),
    );
    let on_change: Rc<dyn Fn(f32, &mut App)> = Rc::new(on_change);
    let frac = if max > min {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let send = {
        let (state, on_change) = (state.clone(), on_change.clone());
        Rc::new(move |x: Pixels, cx: &mut App| {
            let v = value_at(state.read(cx).track, x, min, max, step);
            if state.read(cx).last != Some(v) {
                state.update(cx, |d, _| d.last = Some(v));
                on_change(v, cx);
            }
        })
    };
    let knob = t.s(12.);
    let (record, watch) = (state.clone(), state.clone());
    let press = (state.clone(), send.clone());
    let track = div()
        .id(ElementId::Name(format!("slider-{id}-track").into()))
        .relative()
        .w(t.s(150.))
        .h(t.s(20.))
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(t.s(8.))
                .h(t.s(4.))
                .rounded_full()
                .bg(t.border_strong),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .w(relative(frac))
                .top(t.s(8.))
                .h(t.s(4.))
                .rounded_full()
                .bg(t.accent),
        )
        .child(
            // The knob's center sits on the value.
            div()
                .absolute()
                .top(t.s(4.))
                .left(relative(frac))
                .ml(-knob * 0.5)
                .size(knob)
                .rounded_full()
                .bg(t.accent_text)
                .border_2()
                .border_color(t.accent),
        )
        .child(
            canvas(
                move |b, _, cx| record.update(cx, |d, _| d.track = b),
                move |_, _, window, cx| {
                    if !watch.read(cx).on {
                        return;
                    }
                    let (moving, send) = (watch.clone(), send.clone());
                    window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        if e.pressed_button != Some(MouseButton::Left) {
                            moving.update(cx, |d, cx| {
                                d.on = false;
                                cx.notify();
                            });
                            return;
                        }
                        send(e.position.x, cx);
                    });
                    let releasing = watch.clone();
                    window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Capture && e.button == MouseButton::Left {
                            releasing.update(cx, |d, cx| {
                                d.on = false;
                                cx.notify();
                            });
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        )
        .on_mouse_down(MouseButton::Left, move |e, window, cx| {
            let (state, send) = &press;
            state.update(cx, |d, cx| {
                d.on = true;
                d.last = None;
                cx.notify();
            });
            send(e.position.x, cx);
            window.refresh();
        });
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(track)
        .child(
            div()
                .min_w(px(44.))
                .text_size(t.ui_size)
                .text_color(t.text_muted)
                .child(label),
        )
        .into_any_element()
}

/// The value at `x` on a track at `track`, snapped to `step`.
fn value_at(track: Bounds<Pixels>, x: Pixels, min: f32, max: f32, step: f32) -> f32 {
    let w = f32::from(track.size.width).max(1.0);
    let f = (f32::from(x - track.origin.x) / w).clamp(0.0, 1.0);
    let v = min + f * (max - min);
    let v = if step > 0.0 {
        min + ((v - min) / step).round() * step
    } else {
        v
    };
    v.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[test]
    fn snaps_and_clamps_to_the_track() {
        let track = Bounds {
            origin: point(px(100.), px(0.)),
            size: size(px(200.), px(20.)),
        };
        assert_eq!(value_at(track, px(100.), 0.0, 1.0, 0.05), 0.0);
        assert_eq!(value_at(track, px(300.), 0.0, 1.0, 0.05), 1.0);
        assert_eq!(value_at(track, px(50.), 0.0, 1.0, 0.05), 0.0);
        assert_eq!(value_at(track, px(900.), 0.0, 1.0, 0.05), 1.0);
        let mid = value_at(track, px(201.), 0.25, 3.0, 0.05);
        assert!((mid - 1.65).abs() < 1e-4, "{mid}");
    }
}
