//! Motion for panels leaving and joining windows: the squash and stretch a panel shows as
//! it lands, and handing a press that is still down to the system so a torn-off window
//! follows the pointer.

use std::time::Duration;

use gpui::Window;

/// How long a panel takes to settle after it lands in a window.
pub const SETTLE: Duration = Duration::from_millis(460);

/// The squash of a landing panel at `t` (0 to 1) of [`SETTLE`]: positive is squashed
/// (wider than tall), negative is stretched, and it rings down to rest at 0.
pub fn squash(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    (1.0 - t).powi(2) * (t * 3.0 * std::f32::consts::PI).cos()
}

/// Insets, as `(horizontal, vertical)` fractions of the panel, that draw `squash`: a
/// squashed panel is pulled in from the top and bottom, a stretched one from the sides.
pub fn insets(squash: f32) -> (f32, f32) {
    let amount = 0.09;
    ((-squash).max(0.0) * amount, squash.max(0.0) * amount * 1.4)
}

/// How a drag ghost is stretched along its motion: the scale along x and along y for a
/// pointer moving at `velocity` pixels per frame. Fast moves stretch it along the
/// direction of travel and thin it across, keeping its area.
pub fn stretch(velocity: (f32, f32)) -> (f32, f32) {
    let (vx, vy) = velocity;
    let sx = 1.0 + (vx.abs() / 60.0).min(0.35) - (vy.abs() / 120.0).min(0.15);
    let sy = 1.0 + (vy.abs() / 60.0).min(0.35) - (vx.abs() / 120.0).min(0.15);
    (sx, sy)
}

/// Lets a newly opened window carry on the press that tore its panel off, so it follows
/// the pointer until the button comes up. Returns whether the system took the move.
pub fn follow_pointer(window: &Window) -> bool {
    #[cfg(windows)]
    {
        windows_follow(window)
    }
    #[cfg(not(windows))]
    {
        window.start_window_move();
        true
    }
}

/// Windows has no call to start a move from code, so the window asks for the same move
/// a press on its caption starts, while the button is still down.
#[cfg(windows)]
fn windows_follow(window: &Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, ReleaseCapture, VK_LBUTTON,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        HTCAPTION, PostMessageW, SC_MOVE, WM_SYSCOMMAND,
    };
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return false;
    };
    // SAFETY: plain Win32 calls on a window this process owns, on its own thread.
    unsafe {
        if GetAsyncKeyState(i32::from(VK_LBUTTON.0)) >= 0 {
            return false;
        }
        let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
        let _ = ReleaseCapture();
        PostMessageW(
            Some(hwnd),
            WM_SYSCOMMAND,
            WPARAM(SC_MOVE as usize | HTCAPTION as usize),
            LPARAM(0),
        )
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squash_starts_squashed_and_comes_to_rest() {
        assert!(squash(0.0) > 0.9);
        assert!(
            (1..10).any(|i| squash(i as f32 / 10.0) < 0.0),
            "it stretches past rest"
        );
        assert!(squash(1.0).abs() < 1e-6);
        let (x, y) = insets(squash(0.0));
        assert!(y > 0.0 && x == 0.0);
    }

    #[test]
    fn stretch_follows_the_motion() {
        assert_eq!(stretch((0.0, 0.0)), (1.0, 1.0));
        let (sx, sy) = stretch((40.0, 0.0));
        assert!(sx > 1.0 && sy < 1.0);
        let (sx, sy) = stretch((1000.0, 0.0));
        assert!(sx <= 1.35 && sy >= 0.85);
    }
}
