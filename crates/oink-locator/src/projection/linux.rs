use super::*;
use crate::linux_x11::{Connection, Window};
use std::rc::Rc;
pub(super) struct Backend {
    connection: Rc<Connection>,
    windows: Vec<Window>,
    health: Arc<crate::diagnostics::Health>,
}
impl Backend {
    pub(super) fn new(health: Arc<crate::diagnostics::Health>) -> Option<Self> {
        Some(Self {
            connection: Connection::open()?,
            windows: vec![],
            health,
        })
    }
    pub(super) fn present(&mut self, frames: Vec<Frame>) {
        while self.windows.len() < frames.len() {
            let Some(w) = Window::new(&self.connection) else {
                self.health.failed();
                return;
            };
            self.windows.push(w);
        }
        for (i, window) in self.windows.iter().enumerate() {
            if let Some(f) = frames.get(i) {
                if !window.present(f.bounds, f.width, f.height, &f.pixels, false) {
                    self.health.failed();
                }
            } else {
                window.hide();
            }
        }
    }
    pub(super) fn hide(&mut self) {
        for w in &self.windows {
            w.hide();
        }
    }
    pub(super) fn pump(&mut self) {
        let _ = self.connection.events();
    }
}
pub(super) fn annotation_unit(_screen: Rect) -> f64 {
    1.0
}
