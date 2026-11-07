use super::*;
use crate::linux_x11::{Connection, Window};
use std::{collections::BTreeMap, rc::Rc};
struct ToastWindow {
    window: Window,
    hit: interaction::HitState,
}
pub(super) struct Backend {
    connection: Rc<Connection>,
    windows: BTreeMap<u64, ToastWindow>,
    feedback: Vec<Feedback>,
}
impl Backend {
    pub fn new() -> Option<Self> {
        Some(Self {
            connection: Connection::open()?,
            windows: BTreeMap::new(),
            feedback: vec![],
        })
    }
}
impl Surface for Backend {
    fn present(&mut self, frames: Vec<Frame>) {
        self.windows.retain(|id, _| frames.iter().any(|f| f.id == *id));
        for f in frames {
            if !self.windows.contains_key(&f.id) {
                let Some(window) = Window::new(&self.connection) else {
                    tracing::warn!("cannot allocate X11 toast window");
                    continue;
                };
                self.windows.insert(
                    f.id,
                    ToastWindow {
                        window,
                        hit: Default::default(),
                    },
                );
            }
            let entry = self.windows.get_mut(&f.id).unwrap();
            entry.hit.update_frame(&f);
            if !entry
                .window
                .present(f.bounds, f.width, f.height, &f.pixels, f.interactive)
            {
                tracing::warn!("X11 toast frame presentation failed");
            }
        }
    }
    fn clicks(&mut self) -> Vec<Click> {
        let mut clicks = vec![];
        for event in self.connection.events() {
            let Some(entry) = self.windows.values_mut().find(|w| w.window.id() == event.window) else {
                continue;
            };
            let point = Point::new(event.x as f64, event.y as f64);
            let input = match event.kind {
                1 => interaction::Input::Down(point),
                2 => interaction::Input::Up(point),
                3 => interaction::Input::Move(point),
                4 => interaction::Input::Leave,
                _ => continue,
            };
            let (feedback, click) = entry.hit.input(input);
            self.feedback.push(feedback);
            if let Some(click) = click {
                clicks.push(click);
            }
        }
        clicks
    }
    fn reduced_motion(&self) -> bool {
        false
    }
    fn feedback(&mut self) -> Vec<Feedback> {
        std::mem::take(&mut self.feedback)
    }
}
