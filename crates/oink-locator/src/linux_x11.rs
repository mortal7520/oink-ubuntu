//! Dedicated X11 connections; no Wayland or compositor-free success fallback.
use oink_foundation::Rect;
use std::{
    ffi::{c_char, c_int, c_void},
    ptr::NonNull,
    rc::Rc,
};
unsafe extern "C" {
    fn oink_x_open() -> *mut c_void;
    fn oink_x_close(x: *mut c_void);
    fn oink_x_window(x: *mut c_void) -> *mut c_void;
    fn oink_x_window_close(w: *mut c_void);
    fn oink_x_window_id(w: *mut c_void) -> u64;
    fn oink_x_hide(w: *mut c_void);
    fn oink_x_present(
        w: *mut c_void,
        x: c_int,
        y: c_int,
        width: u32,
        height: u32,
        pixels: *const u32,
        interactive: c_int,
    ) -> c_int;
    fn oink_x_event(x: *mut c_void, window: *mut u64, kind: *mut c_int, px: *mut c_int, py: *mut c_int) -> c_int;
}
#[derive(Debug)]
pub(crate) struct Connection(NonNull<c_void>);
impl Connection {
    pub fn open() -> Option<Rc<Self>> {
        // SAFETY: native constructor owns an independent display and verifies ARGB, XFixes and compositor.
        NonNull::new(unsafe { oink_x_open() }).map(|p| Rc::new(Self(p)))
    }
    pub fn events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let (mut window, mut kind, mut x, mut y) = (0, 0, 0, 0);
            // SAFETY: connection and all output values are live on the creating UI worker.
            if unsafe { oink_x_event(self.0.as_ptr(), &mut window, &mut kind, &mut x, &mut y) } == 0 {
                break;
            }
            out.push(Event { window, kind, x, y });
        }
        out
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: every native window retains this Rc and has already been destroyed.
        unsafe { oink_x_close(self.0.as_ptr()) }
    }
}
#[derive(Debug)]
pub(crate) struct Window {
    pointer: NonNull<c_void>,
    _connection: Rc<Connection>,
}
impl Window {
    pub fn new(connection: &Rc<Connection>) -> Option<Self> {
        // SAFETY: connection is valid and confined to the current worker.
        NonNull::new(unsafe { oink_x_window(connection.0.as_ptr()) }).map(|pointer| Self {
            pointer,
            _connection: connection.clone(),
        })
    }
    pub fn id(&self) -> u64 {
        // SAFETY: native window remains owned by self.
        unsafe { oink_x_window_id(self.pointer.as_ptr()) }
    }
    pub fn hide(&self) {
        // SAFETY: native window remains owned by self.
        unsafe { oink_x_hide(self.pointer.as_ptr()) }
    }
    pub fn present(&self, bounds: Rect, width: usize, height: usize, pixels: &[u32], interactive: bool) -> bool {
        if !bounds.x.is_finite()
            || !bounds.y.is_finite()
            || width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || pixels.len() != width.saturating_mul(height)
        {
            return false;
        }
        // SAFETY: pixel storage contains width*height ARGB values and C only borrows it during XPutImage.
        unsafe {
            oink_x_present(
                self.pointer.as_ptr(),
                bounds.x.round() as i32,
                bounds.y.round() as i32,
                width as u32,
                height as u32,
                pixels.as_ptr(),
                i32::from(interactive),
            ) == 0
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: exclusive native window ownership, with its connection still retained.
        unsafe { oink_x_window_close(self.pointer.as_ptr()) }
    }
}
#[derive(Debug)]
pub(crate) struct Event {
    pub window: u64,
    pub kind: i32,
    pub x: i32,
    pub y: i32,
}
#[repr(C)]
pub(crate) struct NativeMask {
    pub alpha: *mut u8,
    pub width: usize,
    pub height: usize,
    pub cap_top: f64,
    pub advance: f64,
    pub left: f64,
}
unsafe extern "C" {
    pub(crate) fn oink_text_mask(text: *const c_char, cap: f64, weight: c_int, out: *mut NativeMask) -> c_int;
    pub(crate) fn oink_text_mask_free(mask: *mut NativeMask);
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn oink_x_probe(w: *mut c_void, pixel: *mut u32, rectangles: *mut c_int, focusable: *mut c_int) -> c_int;
    }
    #[test]
    #[ignore = "requires an isolated Xvfb display and xcompmgr; set OINK_TEST_X11=1"]
    fn native_argb_and_input_shape() {
        assert_eq!(
            std::env::var("OINK_TEST_X11").as_deref(),
            Ok("1"),
            "never run this against an unapproved desktop"
        );
        let connection = Connection::open().expect("X11 compositor, ARGB visual and XFixes");
        let window = Window::new(&connection).unwrap();
        let pixels = vec![0xff126abcu32; 256];
        assert!(window.present(Rect::new(32.0, 32.0, 16.0, 16.0), 16, 16, &pixels, false));
        let (mut pixel, mut rectangles, mut focusable) = (0, 0, 0);
        // SAFETY: this worker owns the window and three writable output variables.
        assert_eq!(
            unsafe { oink_x_probe(window.pointer.as_ptr(), &mut pixel, &mut rectangles, &mut focusable) },
            0
        );
        assert_eq!(pixel, 0xff126abc, "actual native ARGB surface pixels");
        assert_eq!(rectangles, 0, "trail/projection surfaces must be input transparent");
        assert_eq!(focusable, 0, "overlay must not accept keyboard focus");
        assert!(window.present(Rect::new(32.0, 32.0, 16.0, 16.0), 16, 16, &pixels, true));
        // SAFETY: unchanged live native window and writable outputs.
        assert_eq!(
            unsafe { oink_x_probe(window.pointer.as_ptr(), &mut pixel, &mut rectangles, &mut focusable) },
            0
        );
        assert!(rectangles > 0, "interactive toasts have a native input region");
        assert_eq!(focusable, 0);
        window.hide();
    }
}
