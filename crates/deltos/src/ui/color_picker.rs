//! A color picker: a saturation and brightness square, a hue slider, and an eyedropper
//! that takes the color of whatever pixel the pointer is over, anywhere on the screen.
//!
//! The picker keeps hue, saturation and value itself, so dragging to gray and back
//! keeps the hue. It emits [`ColorEvent::Changed`] as the color moves.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Bounds, Context, EventEmitter, FocusHandle, Hsla, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, Pixels, Point, Render, Window, anchored, canvas, deferred, div,
    linear_color_stop, linear_gradient, point, prelude::*, px,
};

use crate::theme::{color, theme};
use crate::ui::icons::Icon;
use crate::ui::{Tooltip, icon_button, motion, screen_color};

const AREA_W: f32 = 224.;
const AREA_H: f32 = 120.;
const HUE_H: f32 = 12.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorEvent {
    /// The color as 0xRRGGBB.
    Changed(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dragging {
    Area,
    Hue,
}

pub struct ColorPicker {
    /// Hue in degrees, saturation and value from 0 to 1.
    hsv: (f32, f32, f32),
    dragging: Option<Dragging>,
    /// The color under the pointer while the eyedropper is on.
    sampling: Option<u32>,
    area: Rc<Cell<Bounds<Pixels>>>,
    hue: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
}

impl EventEmitter<ColorEvent> for ColorPicker {}

impl ColorPicker {
    pub fn new(rgb: u32, cx: &mut Context<Self>) -> Self {
        Self {
            hsv: rgb_to_hsv(rgb),
            dragging: None,
            sampling: None,
            area: Rc::new(Cell::new(Bounds::default())),
            hue: Rc::new(Cell::new(Bounds::default())),
            focus: cx.focus_handle(),
        }
    }

    pub fn rgb(&self) -> u32 {
        hsv_to_rgb(self.hsv)
    }

    /// Shows `rgb` without emitting, for a color set elsewhere. Keeps the hue of a gray.
    pub fn set_rgb(&mut self, rgb: u32, cx: &mut Context<Self>) {
        if rgb == self.rgb() {
            return;
        }
        let (h, s, v) = rgb_to_hsv(rgb);
        self.hsv = if s == 0.0 || v == 0.0 {
            (self.hsv.0, s, v)
        } else {
            (h, s, v)
        };
        cx.notify();
    }

    fn change(&mut self, hsv: (f32, f32, f32), cx: &mut Context<Self>) {
        let before = self.rgb();
        self.hsv = hsv;
        if self.rgb() != before {
            cx.emit(ColorEvent::Changed(self.rgb()));
        }
        cx.notify();
    }

    fn pick_at(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        let (h, s, v) = self.hsv;
        match self.dragging {
            Some(Dragging::Area) => {
                let b = self.area.get();
                let (fx, fy) = fraction(b, p);
                self.change((h, fx, 1.0 - fy), cx);
            }
            Some(Dragging::Hue) => {
                let (fx, _) = fraction(self.hue.get(), p);
                self.change((fx * 360.0, s, v), cx);
            }
            None => {}
        }
    }

    /// Turns the eyedropper on: the color under the pointer shows as it moves, and a
    /// click anywhere, Enter or Space takes it. Escape puts the color back.
    fn start_sampling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sampling.is_some() {
            return;
        }
        self.sampling = Some(self.rgb());
        window.focus(&self.focus);
        cx.notify();
        cx.spawn(async move |this, cx| {
            // The press that turned it on is still down for a moment; a pick is the next
            // press after it comes up.
            let mut was_down = true;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                let down = motion::left_button_down().unwrap_or(false);
                let pressed = down && !was_down;
                was_down = down;
                let sample = screen_color::under_pointer();
                let go_on = this
                    .update(cx, |this, cx| {
                        if this.sampling.is_none() {
                            return false;
                        }
                        if let Some(c) = sample {
                            this.sampling = Some(c);
                            cx.notify();
                        }
                        if pressed {
                            this.finish_sampling(true, cx);
                            return false;
                        }
                        true
                    })
                    .unwrap_or(false);
                if !go_on {
                    break;
                }
            }
        })
        .detach();
    }

    fn finish_sampling(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some(c) = self.sampling.take() else {
            return;
        };
        if keep {
            let (h, s, v) = rgb_to_hsv(c);
            let h = if s == 0.0 { self.hsv.0 } else { h };
            self.change((h, s, v), cx);
        }
        cx.notify();
    }

    /// Follows the pointer past the square or slider while the button is down. See the
    /// GPUI notes in AGENTS.md for why a canvas does this.
    fn pointer_watch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let moving = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    moving
                        .update(cx, |this, cx| {
                            if this.dragging.is_none() {
                                return;
                            }
                            if e.pressed_button != Some(MouseButton::Left) {
                                this.dragging = None;
                                cx.notify();
                                return;
                            }
                            this.pick_at(e.position, cx);
                        })
                        .ok();
                });
                let releasing = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture && e.button == MouseButton::Left {
                        releasing
                            .update(cx, |this, cx| {
                                if this.dragging.take().is_some() {
                                    cx.notify();
                                }
                            })
                            .ok();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }
}

/// Where `p` falls in `b`, as fractions from 0 to 1 on each axis.
fn fraction(b: Bounds<Pixels>, p: Point<Pixels>) -> (f32, f32) {
    let w = f32::from(b.size.width).max(1.0);
    let h = f32::from(b.size.height).max(1.0);
    (
        (f32::from(p.x - b.origin.x) / w).clamp(0.0, 1.0),
        (f32::from(p.y - b.origin.y) / h).clamp(0.0, 1.0),
    )
}

/// Records an element's bounds as it paints, for turning pointer positions into colors.
fn record(slot: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |b, _, _| slot.set(b), |_, _, _, _| {})
        .absolute()
        .size_full()
}

fn hue_color(h: f32) -> Hsla {
    color(hsv_to_rgb((h, 1.0, 1.0)))
}

impl Render for ColorPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let (h, s, v) = self.hsv;
        let current = self.sampling.unwrap_or_else(|| self.rgb());
        let knob = |size: f32| {
            div()
                .absolute()
                .size(px(size))
                .rounded_full()
                .border_2()
                .border_color(gpui::white())
                .shadow_sm()
        };
        let white = gpui::white();
        let black = gpui::black();
        let area = div()
            .id("color-area")
            .relative()
            .w(px(AREA_W))
            .h(px(AREA_H))
            .rounded(t.radius_small)
            .overflow_hidden()
            .bg(hue_color(h))
            .cursor_crosshair()
            .child(record(self.area.clone()))
            .child(div().absolute().size_full().bg(linear_gradient(
                90.,
                linear_color_stop(white, 0.),
                linear_color_stop(white.opacity(0.), 1.),
            )))
            .child(div().absolute().size_full().bg(linear_gradient(
                180.,
                linear_color_stop(black.opacity(0.), 0.),
                linear_color_stop(black, 1.),
            )))
            .child(
                knob(12.)
                    .left(px(s * AREA_W - 6.))
                    .top(px((1.0 - v) * AREA_H - 6.)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.dragging = Some(Dragging::Area);
                    this.pick_at(e.position, cx);
                }),
            );
        let mut hue = div()
            .id("color-hue")
            .relative()
            .w(px(AREA_W))
            .h(px(HUE_H))
            .rounded_full()
            .flex()
            .cursor_pointer()
            .child(record(self.hue.clone()));
        for i in 0..6 {
            let from = hue_color(i as f32 * 60.0);
            let to = hue_color((i + 1) as f32 * 60.0);
            hue = hue.child(
                div()
                    .flex_1()
                    .h_full()
                    .when(i == 0, |el| el.rounded_l(px(HUE_H / 2.)))
                    .when(i == 5, |el| el.rounded_r(px(HUE_H / 2.)))
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(from, 0.),
                        linear_color_stop(to, 1.),
                    )),
            );
        }
        let hue = hue
            .child(
                knob(HUE_H + 2.)
                    .top(px(-1.))
                    .left(px(h / 360.0 * AREA_W - (HUE_H + 2.) / 2.))
                    .bg(hue_color(h)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.dragging = Some(Dragging::Hue);
                    this.pick_at(e.position, cx);
                }),
            );
        let sampling = self.sampling.is_some();
        let dropper = screen_color::available().then(|| {
            icon_button("color-dropper", Icon::Eyedropper, &t)
                .when(sampling, |el| {
                    el.bg(t.accent.opacity(0.18)).text_color(t.accent)
                })
                .tooltip(|_, cx| {
                    Tooltip::view(
                        "Take a color from the screen: move over any pixel, then click",
                        cx,
                    )
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.sampling.is_some() {
                        this.finish_sampling(false, cx);
                    } else {
                        this.start_sampling(window, cx);
                    }
                }))
        });
        let footer = div()
            .w(px(AREA_W))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .size(px(20.))
                    .flex_none()
                    .rounded(t.radius_small)
                    .border_1()
                    .border_color(t.border_strong)
                    .bg(color(current)),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(t.small_size)
                    .text_color(if sampling { t.text } else { t.text_muted })
                    .child(if sampling {
                        format!("#{current:06X} · click to take, Esc to cancel")
                    } else {
                        format!("#{current:06X}")
                    }),
            )
            .children(dropper);
        // While sampling, a click inside this window takes the color instead of pressing
        // whatever is under it.
        let shield = sampling.then(|| {
            let size = window.viewport_size();
            deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("color-shield")
                        .w(size.width)
                        .h(size.height)
                        .occlude()
                        .cursor_crosshair()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.finish_sampling(true, cx)),
                        ),
                ),
            )
            .with_priority(10)
        });
        div()
            .id("color-picker")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if this.sampling.is_none() {
                    return;
                }
                match e.keystroke.key.as_str() {
                    "escape" => this.finish_sampling(false, cx),
                    "enter" | "space" => this.finish_sampling(true, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.pointer_watch(cx))
            .child(area)
            .child(hue)
            .child(footer)
            .children(shield)
    }
}

/// 0xRRGGBB as hue in degrees and saturation and value from 0 to 1.
pub fn rgb_to_hsv(rgb: u32) -> (f32, f32, f32) {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.0;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.0;
    let b = (rgb & 0xFF) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

/// Hue in degrees (any value, taken around the circle), saturation and value from 0 to
/// 1, as 0xRRGGBB.
pub fn hsv_to_rgb((h, s, v): (f32, f32, f32)) -> u32 {
    let h = h.rem_euclid(360.0);
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_round_trip() {
        for rgb in [
            0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0x2F7CF6, 0x808080, 0xFF9F0A,
            0x5E5CE6, 0x123456,
        ] {
            assert_eq!(hsv_to_rgb(rgb_to_hsv(rgb)), rgb, "{rgb:06X}");
        }
    }

    #[test]
    fn reads_known_colors() {
        assert_eq!(rgb_to_hsv(0xFF0000), (0.0, 1.0, 1.0));
        assert_eq!(rgb_to_hsv(0x00FF00).0, 120.0);
        assert_eq!(rgb_to_hsv(0x0000FF).0, 240.0);
        assert_eq!(hsv_to_rgb((360.0, 1.0, 1.0)), 0xFF0000);
        assert_eq!(hsv_to_rgb((0.0, 0.0, 0.5)), 0x808080);
    }
}
