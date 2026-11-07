use super::{Mask, StrokeText, TextRaster};
use crate::linux_x11::{NativeMask, oink_text_mask, oink_text_mask_free};
use std::ffi::CString;
#[derive(Debug)]
pub(super) struct SystemText {
    weight: i32,
}
impl SystemText {
    pub fn new() -> Option<Self> {
        Some(Self { weight: 400 })
    }
    pub fn toast() -> Option<Self> {
        Some(Self { weight: 450 })
    }
    pub fn medium() -> Option<Self> {
        Some(Self { weight: 500 })
    }
}
impl TextRaster for SystemText {
    fn line(&self, text: &str, cap: f64, bold: bool) -> Mask {
        let Ok(text) = CString::new(text) else {
            return Mask::default();
        };
        let mut native = NativeMask {
            alpha: std::ptr::null_mut(),
            width: 0,
            height: 0,
            cap_top: 0.0,
            advance: 0.0,
            left: 0.0,
        };
        // SAFETY: valid NUL-terminated UTF-8 and writable result storage; Pango owns no Rust references.
        if unsafe { oink_text_mask(text.as_ptr(), cap, if bold { 600 } else { self.weight }, &mut native) } != 0 {
            return StrokeText.line(text.to_str().unwrap_or_default(), cap, bold);
        }
        // SAFETY: successful native rendering allocates width*height readable alpha bytes.
        let alpha = unsafe { std::slice::from_raw_parts(native.alpha, native.width * native.height) }.to_vec();
        let mask = Mask {
            w: native.width,
            h: native.height,
            alpha,
            cap_top: native.cap_top,
            advance: native.advance,
            left: native.left,
        };
        // SAFETY: this is exactly the native allocation returned above, freed once after copying.
        unsafe {
            oink_text_mask_free(&mut native);
        }
        mask
    }
}
