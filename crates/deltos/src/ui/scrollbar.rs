//! A vertical scrollbar for a list that scrolls with a GPUI [`ScrollHandle`], such as a
//! `uniform_list` tracked by a `UniformListScrollHandle`. The table grid draws its own,
//! since its height can exceed what `f32` pixels hold; this one is for the panels.
//!
//! The thumb is painted from a `canvas` after the list has laid out in the same frame, so
//! it never lags the wheel by a frame. Pressing the track jumps there and drags; the drag
//! follows the pointer outside the bar and the window through `window.on_mouse_event`.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Bounds, DispatchPhase, Hsla, IntoElement, MouseButton, Pixels, ScrollHandle, Styled, canvas,
    fill, point, px, size,
};

/// Width of the bar's track, which takes presses.
const TRACK: f32 = 10.0;
/// Width of the thumb drawn in it.
const THUMB: f32 = 6.0;
/// The thumb never gets shorter than this, however long the list.
const MIN_THUMB: f32 = 24.0;

/// The bar's state between frames: the handle of the list it scrolls, and the drag.
#[derive(Clone)]
pub struct Scrollbar {
    handle: ScrollHandle,
    /// While the thumb is dragged: where on it the pointer took hold.
    grab: Rc<Cell<Option<f32>>>,
    hovered: Rc<Cell<bool>>,
}

impl Scrollbar {
    pub fn new(handle: ScrollHandle) -> Self {
        Self {
            handle,
            grab: Rc::default(),
            hovered: Rc::default(),
        }
    }

    /// The bar, to lay over the right edge of the list: put it in a relative parent that
    /// holds the list. Nothing shows while everything fits.
    pub fn element(&self, color: Hsla) -> impl IntoElement {
        let this = self.clone();
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let Some(geometry) = Geometry::of(&this.handle, bounds) else {
                    this.grab.set(None);
                    return;
                };
                let active = this.grab.get().is_some() || this.hovered.get();
                let thumb = Bounds::new(
                    point(
                        bounds.right() - px((TRACK + THUMB) / 2.0),
                        bounds.top() + px(geometry.thumb_top),
                    ),
                    size(px(THUMB), px(geometry.thumb_len)),
                );
                window.paint_quad(
                    fill(thumb, color.opacity(if active { 0.85 } else { 0.5 }))
                        .corner_radii(px(THUMB / 2.0)),
                );

                let bar = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || e.button != MouseButton::Left
                        || !bounds.contains(&e.position)
                    {
                        return;
                    }
                    cx.stop_propagation();
                    let local = f32::from(e.position.y - bounds.top());
                    let on_thumb = local >= geometry.thumb_top
                        && local <= geometry.thumb_top + geometry.thumb_len;
                    let grab = if on_thumb {
                        local - geometry.thumb_top
                    } else {
                        geometry.thumb_len / 2.0
                    };
                    bar.grab.set(Some(grab));
                    bar.scroll_to(&geometry, local - grab);
                    window.refresh();
                });
                let bar = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, _| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    if let Some(grab) = bar.grab.get() {
                        if e.pressed_button != Some(MouseButton::Left) {
                            bar.grab.set(None);
                            window.refresh();
                            return;
                        }
                        let local = f32::from(e.position.y - bounds.top());
                        bar.scroll_to(&geometry, local - grab);
                        window.refresh();
                        return;
                    }
                    let over = bounds.contains(&e.position);
                    if over != bar.hovered.get() {
                        bar.hovered.set(over);
                        window.refresh();
                    }
                });
                let bar = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, window, _| {
                    if phase == DispatchPhase::Capture
                        && e.button == MouseButton::Left
                        && bar.grab.take().is_some()
                    {
                        window.refresh();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .w(px(TRACK))
    }

    /// Scrolls so the thumb's top is `top` pixels down the track.
    fn scroll_to(&self, geometry: &Geometry, top: f32) {
        let offset = geometry.offset_for(top);
        let x = self.handle.offset().x;
        self.handle.set_offset(point(x, px(-offset)));
    }
}

/// Where the thumb is, worked out from the list's last layout.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    track: f32,
    thumb_len: f32,
    thumb_top: f32,
    /// How far the list scrolls, in pixels.
    max: f32,
}

impl Geometry {
    fn of(handle: &ScrollHandle, bar: Bounds<Pixels>) -> Option<Self> {
        let viewport = f32::from(handle.bounds().size.height);
        let max = f32::from(handle.max_offset().height);
        let scrolled = -f32::from(handle.offset().y);
        Self::new(f32::from(bar.size.height), viewport, max, scrolled)
    }

    fn new(track: f32, viewport: f32, max: f32, scrolled: f32) -> Option<Self> {
        if !(max > 0.5 && viewport > 0.0 && track > 0.0) {
            return None;
        }
        let thumb_len = (track * viewport / (viewport + max)).clamp(MIN_THUMB.min(track), track);
        let room = track - thumb_len;
        let thumb_top = room * (scrolled / max).clamp(0.0, 1.0);
        Some(Self {
            track,
            thumb_len,
            thumb_top,
            max,
        })
    }

    /// The scroll offset that puts the thumb's top at `top`.
    fn offset_for(&self, top: f32) -> f32 {
        let room = self.track - self.thumb_len;
        if room <= 0.0 {
            return 0.0;
        }
        (top / room).clamp(0.0, 1.0) * self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bar_while_everything_fits() {
        assert_eq!(Geometry::new(400.0, 400.0, 0.0, 0.0), None);
        assert_eq!(Geometry::new(0.0, 400.0, 100.0, 0.0), None);
    }

    #[test]
    fn thumb_follows_the_scroll_and_back() {
        // 400 px showing of 1,600: a quarter of the track.
        let g = Geometry::new(400.0, 400.0, 1200.0, 0.0).unwrap();
        assert_eq!(g.thumb_len, 100.0);
        assert_eq!(g.thumb_top, 0.0);
        let g = Geometry::new(400.0, 400.0, 1200.0, 1200.0).unwrap();
        assert_eq!(g.thumb_top, 300.0);
        assert_eq!(g.offset_for(150.0), 600.0);
        // Past the ends stays at the ends.
        assert_eq!(g.offset_for(-20.0), 0.0);
        assert_eq!(g.offset_for(1000.0), 1200.0);
    }

    #[test]
    fn long_lists_keep_a_thumb_to_hold() {
        let g = Geometry::new(300.0, 300.0, 3_000_000.0, 0.0).unwrap();
        assert_eq!(g.thumb_len, MIN_THUMB);
    }
}
