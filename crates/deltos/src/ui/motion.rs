//! Motion for panels leaving and joining windows: the squash and stretch a panel shows as
//! it lands, and handing a press that is still down to the system so a torn-off window
//! follows the pointer.

use std::time::Duration;

use gpui::{Pixels, Point, Window};

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
/// the pointer until the button comes up. First the window is put so that `grab`, a point
/// in it, is under the pointer: window managers and Windows place a new window where they
/// like, and a move started from there keeps whatever gap that left.
///
/// Returns false when the window is not ready to be placed yet, so the caller should try
/// again shortly. With `last`, the move starts wherever the window is.
pub fn follow_pointer(window: &Window, grab: Point<Pixels>, last: bool) -> bool {
    #[cfg(windows)]
    {
        let _ = last;
        windows_follow(window, grab);
        true
    }
    #[cfg(not(windows))]
    {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if !last && !x11_place(window, grab) {
            return false;
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = (grab, last);
        window.start_window_move();
        true
    }
}

/// On X11, asks the window manager to put the newest of this process's windows so `grab`
/// is under the pointer. Returns false while that window is not mapped yet. Wayland gives
/// clients no say in where a window goes, so there the compositor's move is all there is.
///
/// gpui does not hand out X11 window ids (its `HasWindowHandle` panics on X11), so the
/// window is found by process id in the window manager's client list. X11 ids rise as a
/// connection creates windows, so the highest one is the window just opened.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn x11_place(window: &Window, grab: Point<Pixels>) -> bool {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask};

    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()) {
        return true;
    }
    let scale = window.scale_factor();
    let place = || -> Result<bool, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &[u8]| -> Result<u32, Box<dyn std::error::Error>> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let client_list = atom(b"_NET_CLIENT_LIST")?;
        let pid_atom = atom(b"_NET_WM_PID")?;
        let pid = std::process::id();
        let clients = conn
            .get_property(false, root, client_list, AtomEnum::WINDOW, 0, 4096)?
            .reply()?;
        let mut newest = None;
        for id in clients.value32().into_iter().flatten() {
            let owner = conn
                .get_property(false, id, pid_atom, AtomEnum::CARDINAL, 0, 1)?
                .reply()?;
            if owner.value32().and_then(|mut v| v.next()) == Some(pid) {
                newest = newest.max(Some(id));
            }
        }
        let Some(id) = newest else {
            return Ok(false);
        };
        let pointer = conn.query_pointer(root)?.reply()?;
        let x = i32::from(pointer.root_x) - (f32::from(grab.x) * scale).round() as i32;
        let y = i32::from(pointer.root_y) - (f32::from(grab.y) * scale).round() as i32;
        // Static gravity, so x and y place the window's own area and not its frame; only
        // x and y are set; the request comes from a pager, which window managers honor.
        let flags: u32 = 10 | (1 << 8) | (1 << 9) | (2 << 12);
        let event = ClientMessageEvent::new(
            32,
            id,
            atom(b"_NET_MOVERESIZE_WINDOW")?,
            [flags, x as u32, y as u32, 0, 0],
        );
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(true)
    };
    match place() {
        Ok(placed) => placed,
        Err(e) => {
            tracing::debug!("could not place the window under the pointer: {e}");
            true
        }
    }
}

/// Windows has no call to start a move from code, so the window is put under the pointer
/// and then asks for the same move a press on its caption starts, while the button is
/// still down.
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_follow(window: &Window, grab: Point<Pixels>) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, ReleaseCapture, VK_LBUTTON,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, HTCAPTION, PostMessageW, SC_MOVE, SWP_NOACTIVATE, SWP_NOSIZE,
        SWP_NOZORDER, SetWindowPos, WM_SYSCOMMAND,
    };
    // `Window` has its own `window_handle`, so the trait's is named in full.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return false;
    };
    let scale = window.scale_factor();
    // SAFETY: plain Win32 calls on a window this process owns, on its own thread, with
    // out-pointers to locals.
    unsafe {
        if GetAsyncKeyState(i32::from(VK_LBUTTON.0)) >= 0 {
            return false;
        }
        let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
        // Everything here is in physical pixels, so it holds at any display scale.
        let mut cursor = POINT::default();
        let mut client = POINT::default();
        let mut frame = RECT::default();
        if GetCursorPos(&mut cursor).is_ok()
            && ClientToScreen(hwnd, &mut client).as_bool()
            && GetWindowRect(hwnd, &mut frame).is_ok()
        {
            let gx = (f32::from(grab.x) * scale).round() as i32;
            let gy = (f32::from(grab.y) * scale).round() as i32;
            let dx = cursor.x - (client.x + gx);
            let dy = cursor.y - (client.y + gy);
            let _ = SetWindowPos(
                hwnd,
                None,
                frame.left + dx,
                frame.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
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
