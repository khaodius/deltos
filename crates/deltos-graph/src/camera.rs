//! The camera: which point of the plane sits at the middle of the panel, and how many
//! screen pixels one world unit takes.

use serde::{Deserialize, Serialize};

use crate::Rect;

/// Far enough out to see a few thousand tables at once.
pub const MIN_ZOOM: f32 = 0.02;
/// Close enough to read a column's type at a comfortable size.
pub const MAX_ZOOM: f32 = 2.5;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    /// The world point at the middle of the viewport.
    pub x: f32,
    pub y: f32,
    /// Screen pixels per world unit.
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

/// How much of each object is drawn at a zoom. Text smaller than a few pixels costs time
/// to lay out and cannot be read, so it is left out as the camera pulls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Detail {
    /// Namespaces and object outlines only.
    Outline,
    /// Object names.
    Names,
    /// Column names.
    Columns,
    /// Column names and types.
    Types,
}

impl Camera {
    pub fn new(x: f32, y: f32, zoom: f32) -> Self {
        Self {
            x,
            y,
            zoom: clamp_zoom(zoom),
        }
    }

    pub fn detail(&self) -> Detail {
        match self.zoom {
            z if z < 0.2 => Detail::Outline,
            z if z < 0.42 => Detail::Names,
            z if z < 0.7 => Detail::Columns,
            _ => Detail::Types,
        }
    }

    /// A world point in pixels from the viewport's top left corner.
    pub fn to_screen(&self, (wx, wy): (f32, f32), (vw, vh): (f32, f32)) -> (f32, f32) {
        (
            (wx - self.x) * self.zoom + vw * 0.5,
            (wy - self.y) * self.zoom + vh * 0.5,
        )
    }

    /// A point in pixels from the viewport's top left corner, on the plane.
    pub fn to_world(&self, (sx, sy): (f32, f32), (vw, vh): (f32, f32)) -> (f32, f32) {
        (
            (sx - vw * 0.5) / self.zoom + self.x,
            (sy - vh * 0.5) / self.zoom + self.y,
        )
    }

    pub fn rect_to_screen(&self, r: &Rect, viewport: (f32, f32)) -> Rect {
        let (x, y) = self.to_screen((r.x, r.y), viewport);
        Rect::new(x, y, r.w * self.zoom, r.h * self.zoom)
    }

    /// The part of the plane in view.
    pub fn visible(&self, (vw, vh): (f32, f32)) -> Rect {
        let (w, h) = (vw / self.zoom, vh / self.zoom);
        Rect::new(self.x - w * 0.5, self.y - h * 0.5, w, h)
    }

    /// Moves the view by a drag of `dx`, `dy` screen pixels.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.x -= dx / self.zoom;
        self.y -= dy / self.zoom;
    }

    /// Zooms by `factor`, keeping the world point under `screen` where it is.
    pub fn zoom_at(&mut self, screen: (f32, f32), factor: f32, viewport: (f32, f32)) {
        let before = self.to_world(screen, viewport);
        self.zoom = clamp_zoom(self.zoom * factor);
        let after = self.to_world(screen, viewport);
        self.x += before.0 - after.0;
        self.y += before.1 - after.1;
    }

    /// The camera that shows all of `r` with `margin` screen pixels around it, no closer
    /// than `max_zoom`.
    pub fn fit(r: &Rect, (vw, vh): (f32, f32), margin: f32, max_zoom: f32) -> Camera {
        let (cx, cy) = r.center();
        let aw = (vw - margin * 2.0).max(vw * 0.25).max(1.0);
        let ah = (vh - margin * 2.0).max(vh * 0.25).max(1.0);
        let zoom = (aw / r.w.max(1.0)).min(ah / r.h.max(1.0)).min(max_zoom);
        Camera::new(cx, cy, zoom)
    }

    /// The camera `t` of the way from `self` to `to`. Zoom moves in log space, so a
    /// flight from far out to close in reads as a steady dive; the position follows the
    /// zoom's progress, which keeps the target near the middle while it grows.
    pub fn lerp(&self, to: &Camera, t: f32) -> Camera {
        let t = t.clamp(0.0, 1.0);
        let (a, b) = (self.zoom.ln(), to.zoom.ln());
        let zoom = (a + (b - a) * t).exp();
        let span = 1.0 / to.zoom - 1.0 / self.zoom;
        let s = if span.abs() < 1e-6 {
            t
        } else {
            (1.0 / zoom - 1.0 / self.zoom) / span
        };
        Camera {
            x: self.x + (to.x - self.x) * s,
            y: self.y + (to.y - self.y) * s,
            zoom,
        }
    }

    /// Whether the two would draw the same picture to within a fraction of a pixel.
    pub fn same_as(&self, other: &Camera) -> bool {
        (self.zoom - other.zoom).abs() < 1e-4
            && ((self.x - other.x) * self.zoom).abs() < 0.25
            && ((self.y - other.y) * self.zoom).abs() < 0.25
    }
}

fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (f32, f32) = (800.0, 600.0);

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    #[test]
    fn screen_and_world_are_inverse() {
        let c = Camera::new(120.0, -40.0, 0.37);
        let p = (13.0, 571.0);
        assert!(close(c.to_screen(c.to_world(p, VIEW), VIEW), p));
        assert!(close(c.to_screen((120.0, -40.0), VIEW), (400.0, 300.0)));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let mut c = Camera::new(0.0, 0.0, 1.0);
        let pointer = (700.0, 100.0);
        let under = c.to_world(pointer, VIEW);
        c.zoom_at(pointer, 0.25, VIEW);
        assert!(close(c.to_world(pointer, VIEW), under));
        assert!((c.zoom - 0.25).abs() < 1e-6);
        // Clamped at both ends.
        c.zoom_at(pointer, 1e-9, VIEW);
        assert_eq!(c.zoom, MIN_ZOOM);
        c.zoom_at(pointer, 1e9, VIEW);
        assert_eq!(c.zoom, MAX_ZOOM);
        assert!(close(c.to_world(pointer, VIEW), under));
    }

    #[test]
    fn panning_follows_the_drag() {
        let mut c = Camera::new(0.0, 0.0, 0.5);
        let grabbed = c.to_world((100.0, 100.0), VIEW);
        c.pan(30.0, -20.0);
        assert!(close(c.to_screen(grabbed, VIEW), (130.0, 80.0)));
    }

    #[test]
    fn fit_shows_the_whole_rect() {
        let r = Rect::new(1000.0, 1000.0, 4000.0, 1000.0);
        let c = Camera::fit(&r, VIEW, 40.0, 1.0);
        let v = c.visible(VIEW);
        assert!(v.x <= r.x && v.right() >= r.right());
        assert!(v.y <= r.y && v.bottom() >= r.bottom());
        // A small rect stops at the zoom limit given.
        let c = Camera::fit(&Rect::new(0.0, 0.0, 10.0, 10.0), VIEW, 40.0, 1.2);
        assert_eq!(c.zoom, 1.2);
    }

    #[test]
    fn lerp_ends_where_asked_and_zooms_geometrically() {
        let a = Camera::new(0.0, 0.0, 0.05);
        let b = Camera::new(5000.0, 2000.0, 1.25);
        assert!(a.lerp(&b, 0.0).same_as(&a));
        assert!(a.lerp(&b, 1.0).same_as(&b));
        let mid = a.lerp(&b, 0.5);
        assert!((mid.zoom - (0.05f32 * 1.25).sqrt()).abs() < 1e-4);
        assert!(mid.x > 0.0 && mid.x < 5000.0);
        // Same zoom: a straight slide.
        let c = Camera::new(100.0, 0.0, 0.05);
        assert!((a.lerp(&c, 0.5).x - 50.0).abs() < 1e-3);
    }

    #[test]
    fn detail_drops_with_zoom() {
        assert_eq!(Camera::new(0.0, 0.0, 0.05).detail(), Detail::Outline);
        assert_eq!(Camera::new(0.0, 0.0, 0.3).detail(), Detail::Names);
        assert_eq!(Camera::new(0.0, 0.0, 0.5).detail(), Detail::Columns);
        assert_eq!(Camera::new(0.0, 0.0, 1.0).detail(), Detail::Types);
        assert_eq!(Camera::new(0.0, 0.0, f32::NAN).zoom, 1.0);
    }
}
