//! XI2 relative-pointer metadata and smooth-scroll translation.
use crate::{InputError, InputEvent, ScrollPhase, ScrollUnit};
use oink_foundation::{Point, Rect};
use std::collections::BTreeMap;
use x11rb::protocol::xinput;
pub(super) fn fixed(v: xinput::Fp3232) -> f64 {
    f64::from(v.integral) + f64::from(v.frac) / 4294967296.
}
pub(super) fn values(mask: &[u32], data: &[xinput::Fp3232]) -> BTreeMap<u16, f64> {
    let mut out = BTreeMap::new();
    let mut n = 0;
    for (word, bits) in mask.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) != 0 {
                if let Some(v) = data.get(n) {
                    out.insert((word * 32 + bit) as u16, fixed(*v));
                }
                n += 1;
            }
        }
    }
    out
}
#[derive(Clone)]
struct ScrollAxis {
    number: u16,
    vertical: bool,
    increment: f64,
    previous: f64,
}
#[derive(Clone)]
pub(super) struct Device {
    pub touchpad: bool,
    scroll: Vec<ScrollAxis>,
}
impl Device {
    pub fn read(info: &xinput::XIDeviceInfo) -> Result<Self, InputError> {
        let mut vals = BTreeMap::new();
        for class in &info.classes {
            if let xinput::DeviceClassData::Valuator(v) = &class.data {
                if v.number < 2 && v.mode != xinput::ValuatorMode::RELATIVE {
                    return Err(InputError::Platform(format!(
                        "X11 absolute tablet/touchscreen capture is unsupported (device {})",
                        info.deviceid
                    )));
                }
                vals.insert(v.number, fixed(v.value));
            }
        }
        let mut scroll = Vec::new();
        for class in &info.classes {
            if let xinput::DeviceClassData::Scroll(s) = &class.data {
                let increment = fixed(s.increment);
                if increment != 0. && increment.is_finite() {
                    scroll.push(ScrollAxis {
                        number: s.number,
                        vertical: s.scroll_type == xinput::ScrollType::VERTICAL,
                        increment,
                        previous: vals.get(&s.number).copied().unwrap_or(0.),
                    });
                }
            }
        }
        let name = String::from_utf8_lossy(&info.name).to_lowercase();
        Ok(Self {
            touchpad: name.contains("touchpad") || name.contains("trackpad"),
            scroll,
        })
    }
    pub fn emulated_wheel(&self, button: u32, flags: xinput::PointerEventFlags) -> bool {
        !self.scroll.is_empty()
            && matches!(button, 4..=7)
            && flags.contains(xinput::PointerEventFlags::POINTER_EMULATED)
    }
    pub fn scroll(&mut self, mask: &[u32], data: &[xinput::Fp3232]) -> Option<InputEvent> {
        let vals = values(mask, data);
        let (mut dx, mut dy) = (0., 0.);
        for axis in &mut self.scroll {
            if let Some(&value) = vals.get(&axis.number) {
                let amount = (value - axis.previous) / axis.increment;
                axis.previous = value;
                if axis.vertical {
                    dy -= amount;
                } else {
                    dx -= amount;
                }
            }
        }
        if dx == 0. && dy == 0. {
            None
        } else {
            Some(InputEvent::Scroll {
                dx,
                dy,
                unit: ScrollUnit::Line,
                phase: ScrollPhase::None,
                wheel_steps: Some(Point::new(dx, dy)),
            })
        }
    }
}
pub(super) fn predict(at: Point, delta: Point, fraction: Point, bounds: Rect) -> (Point, Point) {
    let x = (at.x + delta.x + fraction.x).clamp(bounds.x, bounds.x + bounds.w - 1.);
    let y = (at.y + delta.y + fraction.y).clamp(bounds.y, bounds.y + bounds.h - 1.);
    let p = Point::new(x.floor(), y.floor());
    (p, Point::new(x - p.x, y - p.y))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subpixel_motion_accumulates_without_drift() {
        let b = Rect::new(0., 0., 100., 100.);
        let (p, f) = predict(Point::new(10., 10.), Point::new(0.4, 0.), Point::default(), b);
        assert_eq!(p.x, 10.);
        let (p, f) = predict(p, Point::new(0.4, 0.), f, b);
        let (p, _) = predict(p, Point::new(0.4, 0.), f, b);
        assert_eq!(p.x, 11.);
    }
    #[test]
    fn outward_motion_is_clipped_without_a_stale_fraction() {
        let (p, f) = predict(
            Point::new(0., 0.),
            Point::new(-2.5, 0.),
            Point::default(),
            Rect::new(0., 0., 100., 100.),
        );
        assert_eq!(p, Point::default());
        assert_eq!(f, Point::default());
    }
    #[test]
    fn fractional_scroll_preserves_direction() {
        let mut d = Device {
            touchpad: true,
            scroll: vec![ScrollAxis {
                number: 2,
                vertical: true,
                increment: 120.,
                previous: 0.,
            }],
        };
        let e = d.scroll(&[4], &[xinput::Fp3232 { integral: 30, frac: 0 }]).unwrap();
        assert!(matches!(e, InputEvent::Scroll { dy: -0.25, .. }));
        assert!(d.emulated_wheel(4, xinput::PointerEventFlags::POINTER_EMULATED));
    }
}
