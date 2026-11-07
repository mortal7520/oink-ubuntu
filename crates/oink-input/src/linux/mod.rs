//! X11 input backend (XI2 physical observation, XTest injection, native grabs).
//! Physical slave keyboards are grabbed before master processing; Pass is replayed
//! through XTest and Swallow never leaks into local clients or modifier state.
//! Relative slave pointers use the same policy. Local motion preserves source
//! acceleration and uses absolute XTest replay; native master grabs stay intact.
//! XTest wheel delivery is notch-based, with fractional XI2 scroll accumulated.
//! XTEST and identified libvirtualhid software devices stay local and unrouted.
//! Other XI2.1 clients may still observe raw physical input by X11 design;
//! swallowing controls normal native delivery and master state, not observers.
//! Wayland/XWayland are rejected rather than claiming compositor-wide access.
use crate::linux_keymap as keymap;
use crate::{
    CaptureMode, CapturedEvent, Disposition, EventHandler, FeelProfile, InjectEvent, InputBackend, InputError,
    InputEvent, InputPermissions, KeyCode, MouseButton, PermissionState, ScrollPhase, ScrollUnit, SourceKind,
};
use oink_foundation::{Point, Rect};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        xfixes::ConnectionExt as _,
        xinput::{self, ConnectionExt as _},
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};
mod pointer;
fn error(e: impl std::fmt::Display) -> InputError {
    InputError::Platform(format!("X11: {e}"))
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn valid_bounds(r: Rect) -> bool {
    [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) && r.w >= 1. && r.h >= 1. && r.w <= 65535. && r.h <= 65535.
}
fn software_device(name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    name.contains("xtest") || name.starts_with("libvirtualhid ")
}
fn coordinate(v: f64) -> Result<i16, InputError> {
    if !v.is_finite() {
        return Err(error("non-finite pointer coordinate"));
    }
    Ok(v.floor().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16)
}
fn button_number(b: MouseButton) -> Option<u8> {
    match b {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        MouseButton::Back => Some(8),
        MouseButton::Forward => Some(9),
        MouseButton::Other(n) => n.checked_add(1).filter(|n| !matches!(n, 4..=7)),
    }
}
fn button_event(n: u32, down: bool) -> Option<InputEvent> {
    match n {
        4..=7 if !down => None,
        4..=7 => {
            let (dx, dy) = match n {
                4 => (0., 1.),
                5 => (0., -1.),
                6 => (1., 0.),
                _ => (-1., 0.),
            };
            Some(InputEvent::Scroll {
                dx,
                dy,
                unit: ScrollUnit::Line,
                phase: ScrollPhase::None,
                wheel_steps: Some(Point::new(dx, dy)),
            })
        }
        _ => Some(InputEvent::Button {
            button: match n {
                1 => MouseButton::Left,
                2 => MouseButton::Middle,
                3 => MouseButton::Right,
                8 => MouseButton::Back,
                9 => MouseButton::Forward,
                n => MouseButton::Other(u8::try_from(n.checked_sub(1)?).ok()?),
            },
            pressed: down,
        }),
    }
}
fn axes(mask: &[u32], values: &[xinput::Fp3232]) -> Point {
    let mut p = Point::default();
    let mut n = 0;
    for (word, bits) in mask.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) != 0 {
                if let Some(v) = values.get(n) {
                    let f = f64::from(v.integral) + f64::from(v.frac) / 4294967296.;
                    match word * 32 + bit {
                        0 => p.x = f,
                        1 => p.y = f,
                        _ => {}
                    }
                }
                n += 1;
            }
        }
    }
    p
}
#[derive(Default)]
struct Keys {
    from: HashMap<u8, KeyCode>,
    to: HashMap<KeyCode, u8>,
}
impl Keys {
    fn read(conn: &RustConnection) -> Result<Self, InputError> {
        let names = conn
            .xkb_get_names(xkb::ID::USE_CORE_KBD.into(), xkb::NameDetail::KEY_NAMES)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let mut out = Self::default();
        if let Some(keys) = names.value_list.key_names {
            for (i, key) in keys.into_iter().enumerate() {
                let native = u16::from(names.first_key) + i as u16;
                if native > 255 {
                    continue;
                }
                if let Some(hid) = keymap::from_xkb_name(key.name) {
                    out.from.insert(native as u8, hid);
                    out.to.entry(hid).or_insert(native as u8);
                }
            }
        }
        // Numeric evdev fallback is only used for keycodes without a named physical key.
        for native in conn.setup().min_keycode..=conn.setup().max_keycode {
            if !out.from.contains_key(&native) {
                if let Some(hid) = u16::from(native).checked_sub(8).and_then(keymap::from_evdev) {
                    out.from.insert(native, hid);
                    out.to.entry(hid).or_insert(native);
                }
            }
        }
        if out.to.is_empty() {
            return Err(error("XKB returned no supported physical keys"));
        }
        Ok(out)
    }
}
struct Native {
    mode: CaptureMode,
    keyboard_remote: bool,
    pointer_grab: bool,
    keyboard_grab: bool,
    hidden: bool,
    confine: Option<u32>,
    constraint: Option<Rect>,
    local_buttons: BTreeSet<(u16, u8)>,
    local_motion: Point,
    local_scroll: Point,
    source_returned: bool,
    local_keys: BTreeMap<(u16, u8), u8>,
    drag: Option<oink_foundation::dnd::DragAnchor>,
    barriers: Vec<u32>,
    held: BTreeMap<(u16, u8), KeyCode>,
    injected_keys: BTreeSet<u8>,
    injected_buttons: BTreeSet<u8>,
    scroll: Point,
    relative: Point,
    last_activity: Option<Instant>,
}
impl Default for Native {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Local,
            keyboard_remote: false,
            pointer_grab: false,
            keyboard_grab: false,
            hidden: false,
            confine: None,
            constraint: None,
            local_buttons: BTreeSet::new(),
            local_motion: Point::default(),
            local_scroll: Point::default(),
            source_returned: false,
            local_keys: BTreeMap::new(),
            drag: None,
            barriers: Vec::new(),
            held: BTreeMap::new(),
            injected_keys: BTreeSet::new(),
            injected_buttons: BTreeSet::new(),
            scroll: Point::default(),
            relative: Point::default(),
            last_activity: None,
        }
    }
}
struct Shared {
    conn: Arc<RustConnection>,
    root: u32,
    keys: Mutex<Keys>,
    keyboards: Mutex<HashSet<u16>>,
    pointers: Mutex<HashMap<u16, pointer::Device>>,
    native: Mutex<Native>,
    active: AtomicBool,
    safety: crate::session_gate::SessionGate,
}
impl Shared {
    fn select_capture_events(&self, enabled: bool, watch_devices: bool) -> Result<(), InputError> {
        let mut mask = xinput::XIEventMask::default();
        if enabled {
            mask |= xinput::XIEventMask::RAW_MOTION;
        }
        if watch_devices {
            mask |= xinput::XIEventMask::HIERARCHY;
        }
        self.conn
            .xinput_xi_select_events(
                self.root,
                &[
                    xinput::EventMask {
                        deviceid: xinput::Device::ALL_MASTER.into(),
                        mask: vec![],
                    },
                    xinput::EventMask {
                        deviceid: xinput::Device::ALL.into(),
                        mask: vec![mask],
                    },
                ],
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn.flush().map_err(error)
    }
    fn grab_keyboards(&self) -> Result<(), InputError> {
        let devices = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let present: HashSet<_> = devices.infos.iter().filter(|d| d.enabled).map(|d| d.deviceid).collect();
        let mut owned = lock(&self.keyboards);
        owned.retain(|id| present.contains(id));
        for d in devices.infos {
            if !d.enabled
                || d.type_ != xinput::DeviceType::SLAVE_KEYBOARD
                || software_device(&d.name)
                || owned.contains(&d.deviceid)
            {
                continue;
            }
            // XI2 temporarily detaches a grabbed slave from its master. Swallowed input
            // therefore never reaches the master key/modifier state or local clients.
            let mask = u32::from(xinput::XIEventMask::KEY_PRESS | xinput::XIEventMask::KEY_RELEASE);
            let status = self
                .conn
                .xinput_xi_grab_device(
                    self.root,
                    CURRENT_TIME,
                    NONE,
                    d.deviceid,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    xinput::GrabOwner::NO_OWNER,
                    &[mask],
                )
                .map_err(error)?
                .reply()
                .map_err(error)?
                .status;
            if status != xproto::GrabStatus::SUCCESS {
                return Err(error(format!(
                    "physical keyboard grab refused for device {}: {status:?}",
                    d.deviceid
                )));
            }
            owned.insert(d.deviceid);
        }
        self.conn.flush().map_err(error)
    }
    fn grab_pointers(&self) -> Result<(), InputError> {
        let devices = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let present: HashSet<_> = devices.infos.iter().filter(|d| d.enabled).map(|d| d.deviceid).collect();
        let mut owned = lock(&self.pointers);
        owned.retain(|id, _| present.contains(id));
        for d in devices.infos {
            if !d.enabled
                || d.type_ != xinput::DeviceType::SLAVE_POINTER
                || software_device(&d.name)
                || owned.contains_key(&d.deviceid)
            {
                continue;
            }
            let info = pointer::Device::read(&d)?;
            let mask = u32::from(
                xinput::XIEventMask::BUTTON_PRESS
                    | xinput::XIEventMask::BUTTON_RELEASE
                    | xinput::XIEventMask::MOTION
                    | xinput::XIEventMask::RAW_MOTION,
            );
            let status = self
                .conn
                .xinput_xi_grab_device(
                    self.root,
                    CURRENT_TIME,
                    NONE,
                    d.deviceid,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    xinput::GrabOwner::NO_OWNER,
                    &[mask],
                )
                .map_err(error)?
                .reply()
                .map_err(error)?
                .status;
            if status != xproto::GrabStatus::SUCCESS {
                return Err(error(format!(
                    "physical pointer grab refused for device {}: {status:?}",
                    d.deviceid
                )));
            }
            owned.insert(d.deviceid, info);
        }
        self.conn.flush().map_err(error)
    }
    // Removal/disable can be followed immediately by reuse of the same XI id.
    // Invalidate old ownership and release its replay leases before re-querying.
    fn forget_devices(&self, event: &xinput::HierarchyEvent) -> Result<(), InputError> {
        let removed: HashSet<_> = event
            .infos
            .iter()
            .filter(|d| {
                u32::from(d.flags)
                    & u32::from(xinput::HierarchyMask::SLAVE_REMOVED | xinput::HierarchyMask::DEVICE_DISABLED)
                    != 0
            })
            .map(|d| d.deviceid)
            .collect();
        if removed.is_empty() {
            return Ok(());
        }
        let mut n = lock(&self.native);
        lock(&self.keyboards).retain(|id| !removed.contains(id));
        lock(&self.pointers).retain(|id, _| !removed.contains(id));
        n.held.retain(|(id, _), _| !removed.contains(id));
        let keys: BTreeSet<_> = n
            .local_keys
            .iter()
            .filter(|((id, _), _)| removed.contains(id))
            .map(|(_, code)| *code)
            .collect();
        n.local_keys.retain(|(id, _), _| !removed.contains(id));
        for key in keys {
            if !n.local_keys.values().any(|v| *v == key) && !n.injected_keys.contains(&key) {
                self.conn
                    .xtest_fake_input(xproto::KEY_RELEASE_EVENT, key, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
        }
        let buttons: BTreeSet<_> = n
            .local_buttons
            .iter()
            .filter(|(id, _)| removed.contains(id))
            .map(|(_, b)| *b)
            .collect();
        let drag = n.drag.is_some();
        n.local_buttons
            .retain(|(id, b)| !removed.contains(id) || (drag && *b == 1));
        for b in buttons {
            if !n.local_buttons.iter().any(|(_, v)| *v == b) && !n.injected_buttons.contains(&b) {
                self.conn
                    .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, b, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
        }
        self.conn.flush().map_err(error)
    }
    fn grab_devices(&self) -> Result<(), InputError> {
        self.grab_keyboards()?;
        self.grab_pointers()
    }
    fn clear_local_buttons(&self, n: &mut Native, preserve_drag: bool) {
        let before: BTreeSet<_> = n.local_buttons.iter().map(|(_, b)| *b).collect();
        n.local_buttons
            .retain(|(_, b)| preserve_drag && n.drag.is_some() && *b == 1);
        for b in before {
            if !n.local_buttons.iter().any(|(_, v)| *v == b) && !n.injected_buttons.contains(&b) {
                let _ = self
                    .conn
                    .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, b, CURRENT_TIME, self.root, 0, 0, 0);
            }
        }
    }
    fn release_pointers(&self, n: &mut Native) {
        self.clear_local_buttons(n, false);
        for (id, _) in lock(&self.pointers).drain() {
            let _ = self.conn.xinput_xi_ungrab_device(CURRENT_TIME, id);
        }
        n.local_motion = Point::default();
        n.local_scroll = Point::default();
    }
    fn pointer_prediction(&self, delta: Point) -> Result<(Point, Point), InputError> {
        let at = self.pointer()?;
        let g = self
            .conn
            .get_geometry(self.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let n = lock(&self.native);
        let mut bounds = Rect::new(0., 0., f64::from(g.width), f64::from(g.height));
        if let Some(r) = n.constraint {
            let x = r.x.max(bounds.x);
            let y = r.y.max(bounds.y);
            let w = (r.x + r.w).min(bounds.x + bounds.w) - x;
            let h = (r.y + r.h).min(bounds.y + bounds.h) - y;
            if w < 1. || h < 1. {
                return Err(error("cursor constraint no longer intersects the X11 root"));
            }
            bounds = Rect::new(x, y, w, h);
        }
        Ok(pointer::predict(at, delta, n.local_motion, bounds))
    }
    fn replay_motion(
        &self,
        at: Point,
        fraction: Point,
        disposition: Disposition,
        queued: crate::session_gate::InputSafety,
    ) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        if disposition == Disposition::Swallow
            || !self.safety.snapshot().accepts(queued)
            || matches!(n.mode, CaptureMode::Remote { .. })
            || n.drag.is_some()
        {
            return Ok(());
        }
        self.conn
            .xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                CURRENT_TIME,
                self.root,
                coordinate(at.x)?,
                coordinate(at.y)?,
                0,
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        n.local_motion = fraction;
        self.conn.flush().map_err(error)
    }
    fn replay_button(
        &self,
        source: u16,
        button: u8,
        pressed: bool,
        disposition: Disposition,
        queued: crate::session_gate::InputSafety,
    ) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        let physical = (source, button);
        if !pressed {
            if n.drag.is_some() && button == 1 {
                return Ok(());
            }
            if n.local_buttons.remove(&physical)
                && !n.local_buttons.iter().any(|(_, b)| *b == button)
                && !n.injected_buttons.contains(&button)
            {
                self.conn
                    .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, button, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
        } else if disposition == Disposition::Pass
            && self.safety.snapshot().accepts(queued)
            && matches!(n.mode, CaptureMode::Local)
            && n.drag.is_none()
        {
            let already = n.local_buttons.iter().any(|(_, b)| *b == button) || n.injected_buttons.contains(&button);
            n.local_buttons.insert(physical);
            if !already {
                self.conn
                    .xtest_fake_input(xproto::BUTTON_PRESS_EVENT, button, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
        }
        self.conn.flush().map_err(error)
    }
    fn scroll_output(&self, n: &mut Native, dx: f64, dy: f64, unit: ScrollUnit, local: bool) -> Result<(), InputError> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(error("non-finite scroll"));
        }
        let factor = if unit == ScrollUnit::Pixel { 1. / 40. } else { 1. };
        let accum = if local { &mut n.local_scroll } else { &mut n.scroll };
        accum.x += dx * factor;
        accum.y += dy * factor;
        for horizontal in [false, true] {
            let amount = if horizontal { accum.x } else { accum.y };
            let count = amount.abs().floor().min(128.) as u16;
            let b = match (horizontal, amount >= 0.) {
                (false, true) => 4,
                (false, false) => 5,
                (true, true) => 6,
                (true, false) => 7,
            };
            for _ in 0..count {
                self.conn
                    .xtest_fake_input(xproto::BUTTON_PRESS_EVENT, b, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
                self.conn
                    .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, b, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
            if horizontal {
                accum.x = amount.fract();
            } else {
                accum.y = amount.fract();
            }
        }
        Ok(())
    }
    fn replay_scroll(
        &self,
        event: &InputEvent,
        disposition: Disposition,
        queued: crate::session_gate::InputSafety,
        reverse: bool,
    ) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        if disposition == Disposition::Swallow
            || !self.safety.snapshot().accepts(queued)
            || matches!(n.mode, CaptureMode::Remote { .. })
            || n.drag.is_some()
        {
            return Ok(());
        }
        if let InputEvent::Scroll { dx, dy, unit, .. } = event {
            let sign = if reverse { -1. } else { 1. };
            self.scroll_output(&mut n, dx * sign, dy * sign, *unit, true)?;
        }
        self.conn.flush().map_err(error)
    }
    fn clear_local_keys(&self, n: &mut Native) {
        let targets: BTreeSet<_> = std::mem::take(&mut n.local_keys).into_values().collect();
        for key in targets {
            if !n.injected_keys.contains(&key) {
                let _ = self
                    .conn
                    .xtest_fake_input(xproto::KEY_RELEASE_EVENT, key, CURRENT_TIME, self.root, 0, 0, 0);
            }
        }
    }
    fn release_keyboards(&self, n: &mut Native) {
        self.clear_local_keys(n);
        for id in lock(&self.keyboards).drain() {
            let _ = self.conn.xinput_xi_ungrab_device(CURRENT_TIME, id);
        }
        n.held.clear();
    }
    fn replay_key(
        &self,
        source: u16,
        native: u8,
        pressed: bool,
        mut disposition: Disposition,
        queued: crate::session_gate::InputSafety,
        rewrite: Option<oink_foundation::input::LocalRewrite>,
    ) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        if !self.safety.snapshot().accepts(queued) {
            disposition = Disposition::Swallow;
        }
        let physical = (source, native);
        let previous = n.local_keys.get(&physical).copied();
        if !pressed || disposition == Disposition::Swallow {
            if let Some(old) = n.local_keys.remove(&physical) {
                if !n.local_keys.values().any(|v| *v == old) && !n.injected_keys.contains(&old) {
                    self.conn
                        .xtest_fake_input(xproto::KEY_RELEASE_EVENT, old, CURRENT_TIME, self.root, 0, 0, 0)
                        .map_err(error)?
                        .check()
                        .map_err(error)?;
                }
            }
            return self.conn.flush().map_err(error);
        }
        let target = match rewrite {
            Some(oink_foundation::input::LocalRewrite::Key { code, .. }) => {
                lock(&self.keys).to.get(&code).copied().ok_or(InputError::Unsupported)?
            }
            _ => native,
        };
        if previous == Some(target) {
            return Ok(());
        } // Native XTest autorepeat already owns this held local key.
        if let Some(old) = n.local_keys.remove(&physical) {
            if !n.local_keys.values().any(|v| *v == old) && !n.injected_keys.contains(&old) {
                self.conn
                    .xtest_fake_input(xproto::KEY_RELEASE_EVENT, old, CURRENT_TIME, self.root, 0, 0, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
        }
        let already = n.local_keys.values().any(|v| *v == target) || n.injected_keys.contains(&target);
        n.local_keys.insert(physical, target);
        if !already {
            self.conn
                .xtest_fake_input(xproto::KEY_PRESS_EVENT, target, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(error)?
                .check()
                .map_err(error)?;
        }
        self.conn.flush().map_err(error)
    }
    fn clear_barriers(&self, n: &mut Native) {
        for id in n.barriers.drain(..) {
            let _ = self.conn.xfixes_delete_pointer_barrier(id);
        }
    }
    fn barriers(&self, n: &mut Native, at: Point) -> Result<(), InputError> {
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let x = coordinate(at.x)?;
        let y = coordinate(at.y)?;
        if x < 0 || y < 0 || x as u16 >= geometry.width || y as u16 >= geometry.height {
            return Err(error("drag anchor is outside the X11 root"));
        }
        let (x, y) = (x as u16, y as u16);
        let lines = [
            (x, y, x + 1, y),
            (x, y + 1, x + 1, y + 1),
            (x, y, x, y + 1),
            (x + 1, y, x + 1, y + 1),
        ];
        for (x1, y1, x2, y2) in lines {
            let id = self.conn.generate_id().map_err(error)?;
            let result = self
                .conn
                .xfixes_create_pointer_barrier(
                    id,
                    self.root,
                    x1,
                    y1,
                    x2,
                    y2,
                    x11rb::protocol::xfixes::BarrierDirections::default(),
                    &[],
                )
                .map_err(error)
                .and_then(|c| c.check().map_err(error));
            if let Err(e) = result {
                self.clear_barriers(n);
                let _ = self.conn.flush();
                return Err(e);
            }
            n.barriers.push(id);
        }
        Ok(())
    }
    fn drag_move(&self, generation: u64, at: Point) -> Result<Point, InputError> {
        let x = coordinate(at.x)?;
        let y = coordinate(at.y)?;
        let mut n = lock(&self.native);
        let anchor = n
            .drag
            .filter(|a| a.generation == generation)
            .ok_or_else(|| error("stale source drag generation"))?;
        self.clear_barriers(&mut n);
        let moved = self
            .conn
            .xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, CURRENT_TIME, self.root, x, y, 0)
            .map_err(error)
            .and_then(|c| c.check().map_err(error));
        let observed = self.pointer();
        let actual = observed.unwrap_or(anchor.at);
        let frozen = self.barriers(&mut n, actual);
        n.drag = Some(oink_foundation::dnd::DragAnchor { at: actual, ..anchor });
        n.relative = Point::default();
        n.local_motion = Point::default();
        self.conn.flush().map_err(error)?;
        moved?;
        frozen?;
        if actual != Point::new(f64::from(x), f64::from(y)) {
            return Err(error("native source grab constrained drag relocation"));
        }
        Ok(actual)
    }
    fn pointer(&self) -> Result<Point, InputError> {
        let p = self
            .conn
            .query_pointer(self.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        Ok(Point::new(f64::from(p.root_x), f64::from(p.root_y)))
    }
    fn release_pointer(&self, n: &mut Native) {
        if n.pointer_grab {
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
            n.pointer_grab = false;
        }
        if let Some(w) = n.confine.take() {
            let _ = self.conn.destroy_window(w);
        }
    }
    fn show(&self, n: &mut Native) {
        if n.hidden {
            let _ = self.conn.xfixes_show_cursor(self.root);
            n.hidden = false;
        }
    }
    fn release(&self) {
        let mut n = lock(&self.native);
        self.release_keyboards(&mut n);
        self.release_pointers(&mut n);
        self.clear_barriers(&mut n);
        n.drag = None;
        self.release_pointer(&mut n);
        if n.keyboard_grab {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            n.keyboard_grab = false;
        }
        self.show(&mut n);
        n.mode = CaptureMode::Local;
        n.keyboard_remote = false;
        let _ = self.conn.flush();
        if let Ok(cookie) = self.conn.get_input_focus() {
            let _ = cookie.reply();
        }
    }
    fn confine(&self, n: &mut Native, bounds: Rect) -> Result<(), InputError> {
        if !valid_bounds(bounds) {
            return Err(error("invalid cursor confinement bounds"));
        }
        let w = self.conn.generate_id().map_err(error)?;
        self.conn
            .create_window(
                0,
                w,
                self.root,
                coordinate(bounds.x)?,
                coordinate(bounds.y)?,
                bounds.w.floor() as u16,
                bounds.h.floor() as u16,
                0,
                xproto::WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::default().override_redirect(1),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        if let Err(e) = self
            .conn
            .map_window(w)
            .map_err(error)
            .and_then(|c| c.check().map_err(error))
        {
            let _ = self.conn.destroy_window(w);
            return Err(e);
        }
        let result = self
            .conn
            .grab_pointer(
                false,
                self.root,
                xproto::EventMask::BUTTON_PRESS | xproto::EventMask::BUTTON_RELEASE | xproto::EventMask::POINTER_MOTION,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                w,
                NONE,
                CURRENT_TIME,
            )
            .map_err(error)
            .and_then(|c| c.reply().map_err(error));
        let status = match result {
            Ok(reply) => reply.status,
            Err(e) => {
                let _ = self.conn.destroy_window(w);
                return Err(e);
            }
        };
        if status != xproto::GrabStatus::SUCCESS {
            let _ = self.conn.destroy_window(w);
            return Err(error(format!("pointer grab refused: {status:?}")));
        }
        n.confine = Some(w);
        n.pointer_grab = true;
        Ok(())
    }
    fn mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        if matches!(mode, CaptureMode::Remote { .. }) && !self.safety.snapshot().active() {
            return Err(InputError::NotStarted);
        }
        match mode {
            CaptureMode::Remote { park } => {
                if let Some(anchor) = n.drag {
                    n.mode = CaptureMode::Remote { park: anchor.at };
                    return self.conn.flush().map_err(error);
                }
                self.clear_local_buttons(&mut n, false);
                n.local_motion = Point::default();
                let x = coordinate(park.x)?;
                let y = coordinate(park.y)?;
                if !n.hidden {
                    self.conn
                        .xfixes_hide_cursor(self.root)
                        .map_err(error)?
                        .check()
                        .map_err(error)?;
                    n.hidden = true;
                }
                self.release_pointer(&mut n);
                let result = (|| {
                    self.conn
                        .warp_pointer(NONE, self.root, 0, 0, 0, 0, x, y)
                        .map_err(error)?
                        .check()
                        .map_err(error)?;
                    self.confine(&mut n, Rect::new(f64::from(x), f64::from(y), 1., 1.))
                })();
                if let Err(e) = result {
                    self.release_pointer(&mut n);
                    self.show(&mut n);
                    n.mode = CaptureMode::Local;
                    self.conn.flush().map_err(error)?;
                    return Err(e);
                }
            }
            CaptureMode::Local => {
                if n.drag.is_some() {
                    n.mode = CaptureMode::Local;
                    return Ok(());
                }
                self.release_pointer(&mut n);
                self.show(&mut n);
                n.mode = CaptureMode::Local;
                if let Some(r) = n.constraint {
                    self.confine(&mut n, r)?;
                }
            }
        }
        n.mode = mode;
        self.conn.flush().map_err(error)
    }
    fn keyboard(&self, remote: bool) -> Result<(), InputError> {
        let mut n = lock(&self.native);
        if remote && !n.keyboard_remote {
            self.clear_local_keys(&mut n);
        }
        n.keyboard_remote = remote;
        self.conn.flush().map_err(error)
    }
}
struct Capture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
struct X11Input {
    shared: Arc<Shared>,
    capture: Mutex<Option<Capture>>,
}
impl Drop for X11Input {
    fn drop(&mut self) {
        self.stop();
        self.stop_injection_session();
    }
}
pub(super) fn backend() -> Result<Box<dyn InputBackend>, InputError> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session.eq_ignore_ascii_case("wayland")
        || (std::env::var_os("WAYLAND_DISPLAY").is_some() && !session.eq_ignore_ascii_case("x11"))
    {
        return Err(error(
            "Wayland global capture/injection is unsupported; log in to an X11 session",
        ));
    }
    let (conn, screen) = x11rb::connect(None).map_err(error)?;
    let root = conn.setup().roots[screen].root;
    let v = conn
        .xinput_xi_query_version(2, 1)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    if (v.major_version, v.minor_version) < (2, 1) {
        return Err(error("XInput 2.1 or newer is required"));
    }
    conn.xtest_get_version(2, 2).map_err(error)?.reply().map_err(error)?;
    conn.xfixes_query_version(5, 0).map_err(error)?.reply().map_err(error)?;
    if !conn
        .xkb_use_extension(1, 0)
        .map_err(error)?
        .reply()
        .map_err(error)?
        .supported
    {
        return Err(error("XKB is required for physical keyboard mapping"));
    }
    let keys = Keys::read(&conn)?;
    let shared = Arc::new(Shared {
        conn: Arc::new(conn),
        root,
        keys: Mutex::new(keys),
        keyboards: Mutex::new(HashSet::new()),
        pointers: Mutex::new(HashMap::new()),
        native: Mutex::new(Native::default()),
        active: AtomicBool::new(false),
        safety: crate::session_gate::SessionGate::default(),
    });
    Ok(Box::new(X11Input {
        shared,
        capture: Mutex::new(None),
    }))
}

struct ResumeGrab {
    conn: Arc<RustConnection>,
    root: u32,
    key: u8,
    mods: u16,
}
impl ResumeGrab {
    fn new(shared: &Arc<Shared>, hotkey: crate::capture_loop::ResumeHotkey) -> Result<Self, InputError> {
        let key = lock(&shared.keys)
            .to
            .get(&hotkey.key)
            .copied()
            .ok_or(InputError::Unsupported)?;
        let mods = (if hotkey.shift { 1 } else { 0 })
            | (if hotkey.control { 4 } else { 0 })
            | (if hotkey.alt { 8 } else { 0 })
            | (if hotkey.meta { 64 } else { 0 });
        let grab = Self {
            conn: shared.conn.clone(),
            root: shared.root,
            key,
            mods,
        };
        for extra in [0, 2, 16, 18] {
            grab.conn
                .grab_key(
                    false,
                    grab.root,
                    xproto::ModMask::from(mods | extra),
                    key,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                )
                .map_err(error)?
                .check()
                .map_err(error)?;
        }
        grab.conn.flush().map_err(error)?;
        Ok(grab)
    }
    fn matches(&self, event: &xproto::KeyPressEvent) -> bool {
        event.detail == self.key && (u16::from(event.state) & (1 | 4 | 8 | 64)) == self.mods
    }
}
impl Drop for ResumeGrab {
    fn drop(&mut self) {
        for extra in [0, 2, 16, 18] {
            let _ = self
                .conn
                .ungrab_key(self.key, self.root, xproto::ModMask::from(self.mods | extra));
        }
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.flush();
    }
}

enum Receiver {
    Handler(EventHandler),
    Loop(Box<dyn crate::capture_loop::CaptureLoop>),
}
impl X11Input {
    fn start_receiver(&self, receiver: Receiver) -> Result<(), InputError> {
        let mut capture = lock(&self.capture);
        if capture.is_some() {
            return Err(error("capture already started"));
        }
        while self.shared.conn.poll_for_event().map_err(error)?.is_some() {}
        self.shared.select_capture_events(true, true)?;
        if let Err(e) = self.shared.grab_devices() {
            self.shared.release();
            let _ = self.shared.select_capture_events(false, false);
            return Err(e);
        }
        let shared = self.shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("oink-x11-capture".into()).spawn(move || {
            struct Exit(Arc<Shared>);
            impl Drop for Exit {
                fn drop(&mut self) {
                    crate::capture_loop::capture_available(false);
                    self.0.release();
                    let _ = self.0.select_capture_events(false, false);
                    self.0.active.store(false, Ordering::Release);
                    crate::capture_loop::clear();
                }
            }
            let _exit = Exit(shared.clone());
            let using_loop = matches!(&receiver, Receiver::Loop(_));
            let handler = match receiver {
                Receiver::Handler(h) => Some(h),
                Receiver::Loop(owner) => {
                    let thread = thread::current();
                    let wake: crate::capture_loop::Wake = Arc::new(move || thread.unpark());
                    shared.safety.watch(&wake);
                    crate::capture_loop::install(owner, wake);
                    crate::capture_loop::capture_available(true);
                    None
                }
            };
            shared.active.store(true, Ordering::Release);
            let _ = started_tx.send(());
            let start = Instant::now();
            let mut was_available = true;
            let mut resume_grab: Option<ResumeGrab> = None;
            let mut last_hotkey = None;
            while !done.load(Ordering::Acquire) {
                if using_loop {
                    let (_, stopped) = crate::capture_loop::poll(Instant::now());
                    if stopped {
                        break;
                    }
                }
                let queued_safety = shared.safety.snapshot();
                let available = queued_safety.active() && (!using_loop || crate::capture_loop::capture_requested());
                if !available && was_available {
                    shared.release();
                    if let Err(e) = shared.select_capture_events(false, true) {
                        tracing::warn!(%e,"X11 capture withdrawal failed");
                        return;
                    }
                    if using_loop {
                        crate::capture_loop::capture_detached();
                    }
                }
                if available && !was_available {
                    if let Err(e) = shared.select_capture_events(true, true) {
                        tracing::warn!(%e,"X11 capture resume failed");
                        return;
                    }
                    if let Err(e) = shared.grab_devices() {
                        tracing::warn!(%e,"X11 keyboard capture could not resume");
                        return;
                    }
                    if using_loop {
                        crate::capture_loop::capture_available(true);
                    }
                }
                if available {
                    resume_grab = None;
                    last_hotkey = None;
                } else if using_loop {
                    let requested = crate::capture_loop::paused_hotkey();
                    if requested != last_hotkey {
                        resume_grab = None;
                        last_hotkey = requested;
                        if let Some(hotkey) = requested {
                            match ResumeGrab::new(&shared, hotkey) {
                                Ok(grab) => resume_grab = Some(grab),
                                Err(e) => tracing::warn!(%e,"X11 resume shortcut could not be registered"),
                            }
                        }
                    }
                }
                was_available = available;
                if available && using_loop {
                    let mut n = lock(&shared.native);
                    if crate::capture_loop::keyboard_disabled() {
                        shared.clear_local_keys(&mut n);
                    }
                    if crate::capture_loop::mouse_disabled() {
                        shared.clear_local_buttons(&mut n, true);
                    }
                    let _ = shared.conn.flush();
                }
                let mut had = false;
                loop {
                    let event = match shared.conn.poll_for_event() {
                        Ok(Some(e)) => e,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(%e,"X11 capture disconnected");
                            return;
                        }
                    };
                    had = true;
                    if let Event::XinputHierarchy(e) = &event {
                        if let Err(e) = shared.forget_devices(e) {
                            tracing::warn!(%e,"X11 removed-device cleanup failed");
                            return;
                        }
                        if available {
                            if let Err(e) = shared.grab_devices() {
                                tracing::warn!(%e,"X11 hotplug keyboard grab failed");
                                return;
                            }
                        }
                        continue;
                    }
                    if matches!(event, Event::MappingNotify(_)) {
                        if let Ok(keys) = Keys::read(&shared.conn) {
                            *lock(&shared.keys) = keys;
                        }
                        continue;
                    }
                    if !available {
                        if let Event::KeyPress(e) = &event {
                            if resume_grab.as_ref().is_some_and(|grab| grab.matches(e)) {
                                resume_grab = None;
                                crate::capture_loop::resume_hotkey();
                            }
                        }
                        continue;
                    }
                    let mut produced = Vec::new();
                    let mut source = SourceKind::Mouse;
                    let mut physical_key = None;
                    let mut physical_button = None;
                    let mut motion = None;
                    match event {
                        Event::XinputKeyPress(e) | Event::XinputKeyRelease(e) => {
                            if !lock(&shared.keyboards).contains(&e.sourceid) {
                                continue;
                            }
                            source = SourceKind::Keyboard;
                            let native = e.detail as u8;
                            let code = { lock(&shared.keys).from.get(&native).copied() };
                            if let Some(code) = code {
                                let pressed = e.event_type == xinput::KEY_PRESS_EVENT;
                                if pressed {
                                    lock(&shared.native).held.insert((e.sourceid, native), code);
                                } else {
                                    lock(&shared.native).held.remove(&(e.sourceid, native));
                                }
                                physical_key = Some((e.sourceid, native, pressed));
                                produced.push(InputEvent::Key { code, pressed });
                            } else {
                                let blocked = {
                                    let n = lock(&shared.native);
                                    n.keyboard_remote
                                } || (using_loop && crate::capture_loop::keyboard_key_blocked(None));
                                let disposition = if blocked {
                                    Disposition::Swallow
                                } else {
                                    Disposition::Pass
                                };
                                let _ = shared.replay_key(
                                    e.sourceid,
                                    native,
                                    e.event_type == xinput::KEY_PRESS_EVENT,
                                    disposition,
                                    queued_safety,
                                    None,
                                );
                            }
                        }
                        Event::XinputButtonPress(e) | Event::XinputButtonRelease(e) => {
                            let devices = lock(&shared.pointers);
                            let Some(device) = devices.get(&e.sourceid) else {
                                continue;
                            };
                            if device.emulated_wheel(e.detail, e.flags) {
                                continue;
                            }
                            if device.touchpad {
                                source = SourceKind::Touchpad;
                            }
                            let pressed = e.event_type == xinput::BUTTON_PRESS_EVENT;
                            if let Some(event) = button_event(e.detail, pressed) {
                                if !matches!(e.detail, 4..=7) {
                                    physical_button = Some((e.sourceid, e.detail as u8, pressed));
                                }
                                produced.push(event);
                            }
                        }
                        Event::XinputMotion(e) => {
                            let mut devices = lock(&shared.pointers);
                            let Some(device) = devices.get_mut(&e.sourceid) else {
                                continue;
                            };
                            if device.touchpad {
                                source = SourceKind::Touchpad;
                            }
                            if let Some(event) = device.scroll(&e.valuator_mask, &e.axisvalues) {
                                produced.push(event);
                            }
                        }
                        Event::XinputRawMotion(e) => {
                            let touchpad = {
                                let devices = lock(&shared.pointers);
                                let Some(device) = devices.get(&e.sourceid) else {
                                    continue;
                                };
                                device.touchpad
                            };
                            if touchpad {
                                source = SourceKind::Touchpad;
                            }
                            let accelerated = axes(&e.valuator_mask, &e.axisvalues);
                            let raw = axes(&e.valuator_mask, &e.axisvalues_raw);
                            if accelerated.x != 0. || accelerated.y != 0. {
                                let local = {
                                    let n = lock(&shared.native);
                                    matches!(n.mode, CaptureMode::Local) && n.drag.is_none()
                                };
                                if local {
                                    match shared.pointer_prediction(accelerated) {
                                        Ok(p) => motion = Some(p),
                                        Err(e) => {
                                            tracing::warn!(%e,"X11 local pointer prediction failed");
                                            return;
                                        }
                                    }
                                }
                                produced.push(InputEvent::PointerDelta {
                                    dx: accelerated.x,
                                    dy: accelerated.y,
                                });
                            }
                            if raw.x != 0. || raw.y != 0. {
                                produced.push(InputEvent::RawPointerDelta { dx: raw.x, dy: raw.y });
                            }
                        }
                        _ => {}
                    }
                    for event in produced {
                        let cursor = if source == SourceKind::Keyboard || {
                            let n = lock(&shared.native);
                            matches!(n.mode, CaptureMode::Remote { .. }) || n.drag.is_some()
                        } {
                            None
                        } else {
                            motion.map(|p| p.0).or_else(|| shared.pointer().ok())
                        };
                        let captured = CapturedEvent {
                            physical: true,
                            event,
                            source,
                            time_us: start.elapsed().as_micros() as u64,
                            cursor,
                        };
                        let mut disposition = if let Some(h) = &handler {
                            h(&captured)
                        } else {
                            crate::capture_loop::event(&captured)
                        };

                        if source != SourceKind::Keyboard {
                            if using_loop && crate::capture_loop::mouse_disabled() {
                                disposition = Disposition::Swallow;
                            }
                            let result = if let Some((id, button, pressed)) = physical_button {
                                shared.replay_button(id, button, pressed, disposition, queued_safety)
                            } else if matches!(captured.event, InputEvent::PointerDelta { .. }) {
                                if let Some((at, fraction)) = motion {
                                    shared.replay_motion(at, fraction, disposition, queued_safety)
                                } else {
                                    Ok(())
                                }
                            } else if matches!(captured.event, InputEvent::Scroll { .. }) {
                                let reverse = using_loop
                                    && matches!(
                                        crate::capture_loop::local_rewrite(),
                                        Some(oink_foundation::input::LocalRewrite::ReverseWheel)
                                    );
                                shared.replay_scroll(&captured.event, disposition, queued_safety, reverse)
                            } else {
                                Ok(())
                            };
                            if let Err(e) = result {
                                tracing::warn!(%e,"X11 local pointer replay failed");
                                return;
                            }
                        }
                        // Only physical slave key events reach this path; XTest replay is filtered.
                        if let Some((source, native, pressed)) = physical_key {
                            let blocked = lock(&shared.native).keyboard_remote
                                || (using_loop
                                    && crate::capture_loop::keyboard_key_blocked(match captured.event {
                                        InputEvent::Key { code, .. } => Some(code),
                                        _ => None,
                                    }));
                            if blocked {
                                disposition = Disposition::Swallow;
                            }
                            let rewrite = if using_loop {
                                crate::capture_loop::local_rewrite()
                            } else {
                                None
                            };
                            if let Err(e) =
                                shared.replay_key(source, native, pressed, disposition, queued_safety, rewrite)
                            {
                                tracing::warn!(%e,"X11 local keyboard replay failed");
                                return;
                            }
                        }
                    }
                }
                if !had {
                    thread::park_timeout(Duration::from_millis(2));
                }
            }
        });
        let worker = match worker {
            Ok(worker) => worker,
            Err(e) => {
                self.shared.release();
                let _ = self.shared.select_capture_events(false, false);
                return Err(error(e));
            }
        };
        if started_rx.recv_timeout(Duration::from_secs(3)).is_err() {
            stop.store(true, Ordering::Release);
            worker.thread().unpark();
            let _ = worker.join();
            return Err(error("capture thread did not initialize"));
        }
        *capture = Some(Capture {
            stop,
            worker: Some(worker),
        });
        Ok(())
    }
}
impl InputBackend for X11Input {
    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        self.start_receiver(Receiver::Handler(handler))
    }
    fn start_loop(&self, owner: Box<dyn crate::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        self.start_receiver(Receiver::Loop(owner))
    }
    fn stop(&self) {
        let current = lock(&self.capture).take();
        if let Some(mut c) = current {
            c.stop.store(true, Ordering::Release);
            if let Some(w) = c.worker.take() {
                w.thread().unpark();
                if w.thread().id() != thread::current().id() {
                    let _ = w.join();
                }
            }
        }
        self.shared.active.store(false, Ordering::Release);
        self.shared.release();
        let _ = self.shared.select_capture_events(false, false);
    }
    fn capture_active(&self) -> Option<bool> {
        Some(self.shared.active.load(Ordering::Acquire))
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        if !self.shared.active.load(Ordering::Acquire) && matches!(mode, CaptureMode::Remote { .. }) {
            return Err(InputError::NotStarted);
        }
        self.shared.mode(mode)
    }
    fn set_keyboard_remote(&self, remote: bool) -> Result<(), InputError> {
        if remote && !self.shared.active.load(Ordering::Acquire) {
            return Err(InputError::NotStarted);
        }
        self.shared.keyboard(remote)
    }
    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        let x = coordinate(at.x)?;
        let y = coordinate(at.y)?;
        let mut n = lock(&self.shared.native);
        self.shared.clear_barriers(&mut n);
        n.drag = None;
        self.shared.release_pointer(&mut n);
        let moved = self
            .shared
            .conn
            .warp_pointer(NONE, self.shared.root, 0, 0, 0, 0, x, y)
            .map_err(error)
            .and_then(|c| c.check().map_err(error));
        n.mode = CaptureMode::Local;
        n.local_motion = Point::default();
        self.shared.show(&mut n);
        self.shared.conn.flush().map_err(error)?;
        moved
    }
    fn set_cursor_constraint(&self, bounds: Option<Rect>) -> Result<(), InputError> {
        let mut n = lock(&self.shared.native);
        if let Some(r) = bounds {
            if !valid_bounds(r) {
                return Err(error("invalid cursor confinement bounds"));
            }
            let p = self.shared.pointer()?;
            if p.x < r.x || p.y < r.y || p.x >= r.x + r.w || p.y >= r.y + r.h {
                return Err(error("cursor confinement would move the current pointer"));
            }
        }
        if matches!(n.mode, CaptureMode::Local) && n.drag.is_none() {
            let previous = n.constraint;
            self.shared.release_pointer(&mut n);
            if let Some(r) = bounds {
                if let Err(e) = self.shared.confine(&mut n, r) {
                    if let Some(old) = previous {
                        let _ = self.shared.confine(&mut n, old);
                    }
                    let _ = self.shared.conn.flush();
                    return Err(e);
                }
            }
        }
        n.constraint = bounds;
        self.shared.conn.flush().map_err(error)
    }
    fn freeze_drag_anchor(&self, generation: u64) -> Result<oink_foundation::dnd::DragAnchor, InputError> {
        let mut n = lock(&self.shared.native);
        if let Some(anchor) = n.drag {
            if anchor.generation == generation {
                return Ok(anchor);
            }
            return Err(error("another source drag is already frozen"));
        }
        let pointer = self
            .shared
            .conn
            .query_pointer(self.shared.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        if !pointer.mask.contains(xproto::KeyButMask::BUTTON1) {
            return Err(error("source drag has no held left button"));
        }
        let at = Point::new(f64::from(pointer.root_x), f64::from(pointer.root_y));
        let hidden = n.hidden;
        if !hidden {
            self.shared
                .conn
                .xfixes_hide_cursor(self.shared.root)
                .map_err(error)?
                .check()
                .map_err(error)?;
            n.hidden = true;
        }
        // Barriers retain the application's existing native grab. Never XGrabPointer here.
        if let Err(e) = self.shared.barriers(&mut n, at) {
            if !hidden {
                self.shared.show(&mut n);
            }
            let _ = self.shared.conn.flush();
            return Err(e);
        }
        let anchor = oink_foundation::dnd::DragAnchor {
            generation,
            at,
            surface: 0,
        };
        n.drag = Some(anchor);
        n.source_returned = false;
        n.relative = Point::default();
        self.shared.conn.flush().map_err(error)?;
        Ok(anchor)
    }
    fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        if let Some(anchor) = anchor {
            let n = lock(&self.shared.native);
            if n.drag == Some(anchor) {
                return Ok(());
            }
            return Err(error("source drag must be frozen locally before setting its anchor"));
        }
        let mode = {
            let mut n = lock(&self.shared.native);
            self.shared.clear_barriers(&mut n);
            n.drag = None;
            if n.source_returned {
                n.mode = CaptureMode::Local;
                n.source_returned = false;
                self.shared.release_pointer(&mut n);
                self.shared.show(&mut n);
                return self.shared.conn.flush().map_err(error);
            }
            n.mode
        };
        self.shared.mode(mode)
    }
    fn relocate_drag_anchor(&self, generation: u64, at: Point) -> Result<(), InputError> {
        self.shared.drag_move(generation, at).map(|_| ())
    }
    fn move_held_drag(&self, generation: u64, at: Point) -> Result<(), InputError> {
        self.shared.drag_move(generation, at)?;
        lock(&self.shared.native).source_returned = true;
        Ok(())
    }
    fn move_held_drag_observed(&self, generation: u64, at: Point, posted: &dyn Fn(Point)) -> Result<(), InputError> {
        let actual = self.shared.drag_move(generation, at)?;
        lock(&self.shared.native).source_returned = true;
        posted(actual);
        Ok(())
    }
    fn press_drag_at(&self, at: Point) -> Result<(), InputError> {
        self.inject(&InjectEvent::MoveTo { pos: at })?;
        self.inject(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        })
    }
    fn release_drag_at(&self, at: Point) -> Result<(), InputError> {
        self.inject(&InjectEvent::MoveTo { pos: at })?;
        self.inject(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        })
    }
    fn release_physical_drag(&self) -> Result<(), InputError> {
        let mut n = lock(&self.shared.native);
        n.local_buttons.retain(|(_, b)| *b != 1);
        n.injected_buttons.remove(&1);
        self.shared
            .conn
            .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, CURRENT_TIME, self.shared.root, 0, 0, 0)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.shared.conn.flush().map_err(error)
    }
    fn cursor_position(&self) -> Result<Point, InputError> {
        self.shared.pointer()
    }
    fn input_safety(&self) -> crate::session_gate::InputSafety {
        self.shared.safety.snapshot()
    }
    fn watch_input_safety(&self, wake: &crate::capture_loop::Wake) {
        self.shared.safety.watch(wake);
    }
    fn set_input_paused(&self, paused: bool) {
        self.shared.safety.set_paused(paused);
        if paused {
            self.shared.release();
        }
    }
    fn held_session_keys(&self) -> Vec<KeyCode> {
        lock(&self.shared.native)
            .held
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    fn system_button_state(&self, button: MouseButton) -> Option<bool> {
        let number = button_number(button)?;
        let bit = match number {
            1 => xproto::KeyButMask::BUTTON1,
            2 => xproto::KeyButMask::BUTTON2,
            3 => xproto::KeyButMask::BUTTON3,
            4 => xproto::KeyButMask::BUTTON4,
            5 => xproto::KeyButMask::BUTTON5,
            _ => return None,
        };
        let p = self.shared.conn.query_pointer(self.shared.root).ok()?.reply().ok()?;
        Some(p.mask.contains(bit))
    }
    fn start_injection_session(&self) -> Result<(), InputError> {
        if self.shared.safety.snapshot().active() {
            Ok(())
        } else {
            Err(InputError::NotStarted)
        }
    }
    fn injection_available(&self) -> Option<bool> {
        Some(self.shared.safety.snapshot().active())
    }
    fn stop_injection_session(&self) {
        let mut n = lock(&self.shared.native);
        for code in std::mem::take(&mut n.injected_keys) {
            if !n.local_keys.values().any(|v| *v == code) {
                let _ = self.shared.conn.xtest_fake_input(
                    xproto::KEY_RELEASE_EVENT,
                    code,
                    CURRENT_TIME,
                    self.shared.root,
                    0,
                    0,
                    0,
                );
            }
        }
        for button in std::mem::take(&mut n.injected_buttons) {
            if !n.local_buttons.iter().any(|(_, b)| *b == button) {
                let _ = self.shared.conn.xtest_fake_input(
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
                    CURRENT_TIME,
                    self.shared.root,
                    0,
                    0,
                    0,
                );
            }
        }
        n.scroll = Point::default();
        n.relative = Point::default();
        let _ = self.shared.conn.flush();
        if let Ok(cookie) = self.shared.conn.get_input_focus() {
            let _ = cookie.reply();
        }
    }
    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        if !self.shared.safety.snapshot().active() {
            return Err(InputError::NotStarted);
        }
        let mut n = lock(&self.shared.native);
        let conn = &self.shared.conn;
        let root = self.shared.root;
        if n.drag.is_some()
            && matches!(
                event,
                InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. } | InjectEvent::MoveRelative { .. }
            )
        {
            return Err(error("ordinary motion is fenced by an active source drag"));
        }
        match *event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                conn.xtest_fake_input(
                    xproto::MOTION_NOTIFY_EVENT,
                    0,
                    CURRENT_TIME,
                    root,
                    coordinate(pos.x)?,
                    coordinate(pos.y)?,
                    0,
                )
                .map_err(error)?
                .check()
                .map_err(error)?;
            }
            InjectEvent::MoveRelative { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(error("non-finite relative motion"));
                }
                n.relative.x += dx;
                n.relative.y += dy;
                let x = coordinate(n.relative.x.trunc())?;
                let y = coordinate(n.relative.y.trunc())?;
                n.relative.x -= f64::from(x);
                n.relative.y -= f64::from(y);
                conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 1, CURRENT_TIME, root, x, y, 0)
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
            InjectEvent::Key { code, pressed }
            | InjectEvent::KeyWithoutLock { code, pressed }
            | InjectEvent::Volume { code, pressed, .. } => {
                if code == KeyCode::MENU_MASK {
                    return Ok(());
                }
                let native = lock(&self.shared.keys)
                    .to
                    .get(&code)
                    .copied()
                    .ok_or(InputError::Unsupported)?;
                let local = n.local_keys.values().any(|v| *v == native);
                let was = n.injected_keys.contains(&native);
                if pressed {
                    if !was && !local {
                        conn.xtest_fake_input(xproto::KEY_PRESS_EVENT, native, CURRENT_TIME, root, 0, 0, 0)
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                    }
                    n.injected_keys.insert(native);
                } else {
                    n.injected_keys.remove(&native);
                    if !local {
                        conn.xtest_fake_input(xproto::KEY_RELEASE_EVENT, native, CURRENT_TIME, root, 0, 0, 0)
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                    }
                }
            }
            InjectEvent::Button { button, pressed } => {
                let native = button_number(button).ok_or(InputError::Unsupported)?;
                let local = n.local_buttons.iter().any(|(_, b)| *b == native);
                let was = n.injected_buttons.contains(&native);
                if pressed {
                    if !local && !was {
                        conn.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, native, CURRENT_TIME, root, 0, 0, 0)
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                    }
                    n.injected_buttons.insert(native);
                } else {
                    n.injected_buttons.remove(&native);
                    if !local {
                        conn.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, native, CURRENT_TIME, root, 0, 0, 0)
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                    }
                }
            }
            InjectEvent::Scroll { dx, dy, unit, .. } => {
                self.shared.scroll_output(&mut n, dx, dy, unit, false)?;
            }
            InjectEvent::TouchpadScroll { wheel, .. } => {
                drop(n);
                return self.inject(&InjectEvent::Scroll {
                    dx: wheel.dx,
                    dy: wheel.dy,
                    unit: wheel.unit,
                    phase: wheel.phase,
                });
            }
            InjectEvent::Gesture(_) | InjectEvent::TouchpadContacts(_) => return Err(InputError::Unsupported),
        }
        conn.flush().map_err(error)
    }
    fn declare_user_activity(&self) -> Result<bool, InputError> {
        let mut n = lock(&self.shared.native);
        if n.last_activity.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return Ok(false);
        }
        self.shared
            .conn
            .force_screen_saver(xproto::ScreenSaver::RESET)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.shared.conn.flush().map_err(error)?;
        n.last_activity = Some(Instant::now());
        Ok(true)
    }
    fn reset_user_idle(&self) -> Result<bool, InputError> {
        self.declare_user_activity()
    }
    fn feel_profile(&self) -> FeelProfile {
        FeelProfile {
            wheel_lines_per_notch: Some(3.),
            ..FeelProfile::default()
        }
    }
    fn permissions(&self) -> InputPermissions {
        InputPermissions {
            accessibility: PermissionState::Granted,
            input_monitoring: PermissionState::Granted,
            secure_input_active: false,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wheel_buttons_have_matching_signs() {
        assert!(matches!(button_event(4, true), Some(InputEvent::Scroll { dy: 1., .. })));
        assert!(matches!(
            button_event(7, true),
            Some(InputEvent::Scroll { dx: -1., .. })
        ));
        assert!(button_event(5, false).is_none());
    }
    #[test]
    fn button_roundtrip() {
        for (native, button) in [
            (1, MouseButton::Left),
            (2, MouseButton::Middle),
            (3, MouseButton::Right),
            (8, MouseButton::Back),
            (9, MouseButton::Forward),
        ] {
            assert_eq!(button_number(button), Some(native));
            assert_eq!(
                button_event(u32::from(native), true),
                Some(InputEvent::Button { button, pressed: true })
            );
        }
    }
    #[test]
    fn sparse_valuator_masks_keep_axis_identity() {
        let p = axes(
            &[2],
            &[xinput::Fp3232 {
                integral: -2,
                frac: 0x80000000,
            }],
        );
        assert_eq!(p, Point::new(0., -1.5));
    }
    #[test]
    fn software_injection_is_not_a_physical_source() {
        assert!(software_device(b"Virtual core XTEST pointer"));
        assert!(software_device(b"libvirtualhid Mouse (Absolute)"));
        assert!(!software_device(b"USB Optical Mouse"));
    }
    #[test]
    fn invalid_constraints_fail() {
        assert!(!valid_bounds(Rect::new(f64::NAN, 0., 1., 1.)));
        assert!(!valid_bounds(Rect::new(0., 0., 0., 1.)));
    }
    #[test]
    fn non_finite_positions_fail() {
        assert!(coordinate(f64::NAN).is_err());
        assert!(coordinate(f64::INFINITY).is_err());
    }
}
