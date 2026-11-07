use super::*;
use crate::linux_x11::{Connection, Window};
use std::rc::Rc;
pub(crate) fn available() -> bool {
    let ok = Connection::open().is_some();
    if !ok {
        tracing::warn!(
            "X11 cursor overlay unavailable: requires DISPLAY, ARGB visual, XFixes and a compositing window manager"
        );
    }
    ok
}
pub(crate) struct Layer {
    pub surface: usize,
    pub rect: CellRect,
    pub pixels: Vec<u32>,
}
pub(crate) struct Backend {
    connection: Rc<Connection>,
    windows: Vec<Window>,
    displays: Vec<DisplayInfo>,
    health: Arc<diagnostics::Health>,
}
impl Backend {
    pub(crate) fn new(health: Arc<diagnostics::Health>) -> Option<Self> {
        Some(Self {
            connection: Connection::open()?,
            windows: Vec::new(),
            displays: Vec::new(),
            health,
        })
    }
    pub(crate) fn set_displays(&mut self, displays: &[DisplayInfo]) {
        self.displays = displays.to_vec();
    }
    pub(crate) fn regions(&self, bounds: CellRect, cell: f64) -> Vec<(usize, CellRect)> {
        if !cell.is_finite() || cell <= 0.0 {
            return vec![];
        }
        self.displays
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let r = bounds.intersect(&crate::effect::cells_of(d.bounds, cell));
                (!r.is_empty()).then_some((i, r))
            })
            .collect()
    }
    pub(crate) fn present(&mut self, cell: f64, layers: Vec<Layer>) {
        while self.windows.len() < self.displays.len() {
            let Some(w) = Window::new(&self.connection) else {
                self.health.failed();
                return;
            };
            self.windows.push(w);
        }
        let mut seen = vec![false; self.windows.len()];
        let k = cell.round().clamp(1.0, 16.0) as usize;
        for l in layers {
            let Some(window) = self.windows.get(l.surface) else {
                continue;
            };
            let (w, h) = (l.rect.w as usize * k, l.rect.h as usize * k);
            if w == 0 || h == 0 || w > 8192 || h > 8192 {
                self.health.failed();
                continue;
            }
            let mut pixels = vec![0; w * h];
            upscale_bilinear(&l.pixels, l.rect.w as usize, l.rect.h as usize, k, &mut pixels, w);
            let bounds = oink_foundation::Rect::new(
                (l.rect.x * k as i64) as f64,
                (l.rect.y * k as i64) as f64,
                w as f64,
                h as f64,
            );
            if !window.present(bounds, w, h, &pixels, false) {
                self.health.failed();
            }
            seen[l.surface] = true;
        }
        for (i, w) in self.windows.iter().enumerate() {
            if !seen[i] {
                w.hide();
            }
        }
    }
    pub(crate) fn hide(&mut self) {
        for w in &self.windows {
            w.hide();
        }
    }
    pub(crate) fn wait_frame(&mut self, start: Instant) {
        let _ = self.connection.events();
        if let Some(left) = DEFAULT_FRAME.checked_sub(start.elapsed()) {
            std::thread::sleep(left);
        }
    }
}
