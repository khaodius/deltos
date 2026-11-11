//! Reading the color of the screen pixel under the pointer, for the eyedropper. Works on
//! Windows and X11; Wayland gives apps no way to read the screen without a portal
//! dialog, so there it is not offered.

/// Whether the eyedropper can work on this system.
pub fn available() -> bool {
    if cfg!(windows) {
        return true;
    }
    cfg!(any(target_os = "linux", target_os = "freebsd"))
        && !std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// The color under the pointer as 0xRRGGBB, or `None` when it cannot be read.
pub fn under_pointer() -> Option<u32> {
    #[cfg(windows)]
    {
        windows_pixel()
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        if !available() {
            return None;
        }
        x11_pixel()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "freebsd")))]
    {
        None
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_pixel() -> Option<u32> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{CLR_INVALID, GetDC, GetPixel, ReleaseDC};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    // SAFETY: the screen's device context is taken and given back here, and the
    // out-pointer is a local.
    unsafe {
        let mut p = POINT::default();
        GetCursorPos(&mut p).ok()?;
        let dc = GetDC(None);
        if dc.is_invalid() {
            return None;
        }
        let c = GetPixel(dc, p.x, p.y);
        ReleaseDC(None, dc);
        if c.0 == CLR_INVALID {
            return None;
        }
        // COLORREF is 0x00BBGGRR.
        let (r, g, b) = (c.0 & 0xFF, (c.0 >> 8) & 0xFF, (c.0 >> 16) & 0xFF);
        Some((r << 16) | (g << 8) | b)
    }
}

/// Reads one pixel of the root window, on a connection kept for the thread since the
/// eyedropper asks many times a second.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn x11_pixel() -> Option<u32> {
    use std::cell::RefCell;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};
    use x11rb::rust_connection::RustConnection;

    thread_local! {
        static CONN: RefCell<Option<(RustConnection, u32)>> = const { RefCell::new(None) };
    }
    CONN.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let (conn, screen) = x11rb::connect(None).ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            *slot = Some((conn, root));
        }
        let (conn, root) = slot.as_ref()?;
        let read = || -> Option<u32> {
            let pointer = conn.query_pointer(*root).ok()?.reply().ok()?;
            let image = conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    *root,
                    pointer.root_x,
                    pointer.root_y,
                    1,
                    1,
                    !0,
                )
                .ok()?
                .reply()
                .ok()?;
            if image.depth < 24 || image.data.len() < 3 {
                return None;
            }
            let d = &image.data;
            let msb = conn.setup().image_byte_order == ImageOrder::MSB_FIRST;
            let (r, g, b) = if msb && d.len() >= 4 {
                (d[1], d[2], d[3])
            } else {
                (d[2], d[1], d[0])
            };
            Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
        };
        let color = read();
        if color.is_none() {
            *slot = None;
        }
        color
    })
}
