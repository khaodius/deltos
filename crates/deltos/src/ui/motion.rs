//! Motion for panels leaving and joining windows: the squash and stretch a panel shows as
//! it lands, and handing a press that is still down to the system so a torn-off window
//! follows the pointer.

use std::time::Duration;

use gpui::{Pixels, Point, Window};

/// How long a panel takes to settle after it lands in a window, at normal speed.
pub const SETTLE: Duration = Duration::from_millis(240);

/// How long the drag ghost's stretch takes to ease off once the pointer stops, in seconds
/// at normal speed.
const DRAG_EASE: f32 = 0.09;

/// The squash of a landing panel at `t` (0 to 1) of its settle: positive is squashed
/// (wider than tall), negative is stretched, and it comes to rest at 0. `smoothness` runs
/// from 0 (rings one and a half times) to 1 (eases to rest with no overshoot): it rings
/// fewer times and dies down sooner as smoothness rises.
pub fn squash_smoothed(t: f32, smoothness: f32) -> f32 {
    let (t, s) = (t.clamp(0.0, 1.0), smoothness.clamp(0.0, 1.0));
    // Half-waves of the cosine: 3 rings, 0.5 is a quarter wave that only falls to rest.
    let half_waves = 3.0 - 2.5 * s;
    let decay = 2.0 + 2.0 * s;
    (1.0 - t).powf(decay) * (t * half_waves * std::f32::consts::PI).cos()
}

/// The motion settings, as the views read them from the theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Off with reduced motion: nothing animates.
    pub on: bool,
    pub speed: f32,
    pub smoothness: f32,
    pub squash: f32,
    pub easing: deltos_settings::MotionEasing,
    pub drag_stretch: bool,
    pub outline: bool,
}

impl Motion {
    pub fn new(a: &deltos_settings::Appearance) -> Self {
        Self {
            on: !a.reduced_motion,
            speed: a.motion_speed.clamp(0.25, 3.0),
            smoothness: a.motion_smoothness.clamp(0.0, 1.0),
            squash: a.motion_squash.clamp(0.0, 2.0),
            easing: a.motion_easing,
            drag_stretch: a.motion_drag_stretch,
            outline: a.motion_outline,
        }
    }

    /// How long a landing panel takes to settle.
    pub fn settle(&self) -> Duration {
        SETTLE.div_f32(self.speed)
    }

    /// Whether a landing panel animates at all: with nothing to squash or outline, there
    /// is nothing to draw.
    pub fn settles(&self) -> bool {
        self.on && (self.squash > 0.0 || self.outline)
    }

    /// The squash at `t` (0 to 1) of [`Motion::settle`], eased and scaled by the settings.
    pub fn squash_at(&self, t: f32) -> f32 {
        squash_smoothed(self.easing.apply(t), self.smoothness) * self.squash
    }

    /// How long the drag ghost's stretch takes to ease off, in seconds.
    pub fn drag_ease(&self) -> f32 {
        DRAG_EASE / self.speed
    }

    /// Whether the drag ghost stretches and leans.
    pub fn stretches(&self) -> bool {
        self.on && self.drag_stretch
    }
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

/// Whether the left mouse button is down anywhere on the screen, also while the system
/// moves a window and the app sees no mouse events. `None` where that cannot be asked,
/// as on Wayland.
pub fn left_button_down() -> Option<bool> {
    #[cfg(windows)]
    {
        #[allow(unsafe_code)]
        // SAFETY: reads the global key state; no pointers are involved.
        let state = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(
                windows::Win32::UI::Input::KeyboardAndMouse::VK_LBUTTON.0,
            ))
        };
        Some(state < 0)
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        x11_left_button_down()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "freebsd")))]
    {
        None
    }
}

/// Asks the X server for the pointer's button state, on one connection kept for the
/// thread, since this is asked every few milliseconds while a window is held.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn x11_left_button_down() -> Option<bool> {
    use std::cell::RefCell;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};
    use x11rb::rust_connection::RustConnection;

    thread_local! {
        static CONN: RefCell<Option<(RustConnection, u32)>> = const { RefCell::new(None) };
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()) {
        return None;
    }
    CONN.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let (conn, screen) = x11rb::connect(None).ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            *slot = Some((conn, root));
        }
        let (conn, root) = slot.as_ref()?;
        match conn.query_pointer(*root).ok().and_then(|c| c.reply().ok()) {
            Some(reply) => Some(reply.mask.contains(KeyButMask::BUTTON1)),
            None => {
                // The connection broke; make a new one next time.
                *slot = None;
                None
            }
        }
    })
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

    fn squash(t: f32) -> f32 {
        squash_smoothed(t, 0.0)
    }

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
    fn smooth_settles_never_overshoot() {
        for i in 0..=20 {
            let v = squash_smoothed(i as f32 / 20.0, 1.0);
            assert!(v >= -1e-6, "a fully smooth settle stretches at {i}");
        }
        assert!(squash_smoothed(0.0, 1.0) > 0.9);
        assert!(squash_smoothed(1.0, 0.5).abs() < 1e-6);
        // Smoothness 0 is the springy settle Deltos always had.
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            let old = (1.0 - t).powi(2) * (t * 3.0 * std::f32::consts::PI).cos();
            assert!((squash(t) - old).abs() < 1e-5);
        }
    }

    #[test]
    fn motion_follows_the_settings() {
        let mut a = deltos_settings::Appearance::default();
        let m = Motion::new(&a);
        assert!(m.settles() && m.stretches());
        assert_eq!(m.settle(), SETTLE);
        a.motion_speed = 2.0;
        a.motion_squash = 0.0;
        a.motion_outline = false;
        let m = Motion::new(&a);
        assert_eq!(m.settle(), SETTLE / 2);
        assert!(!m.settles(), "nothing to draw");
        a.reduced_motion = true;
        assert!(!Motion::new(&a).stretches());
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
