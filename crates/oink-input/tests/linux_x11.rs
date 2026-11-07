#![cfg(target_os = "linux")]
//! Opt-in native integration test. Never run against a person's X server.
use oink_foundation::{
    Point,
    input::{CaptureMode, InjectEvent, KeyCode, MouseButton},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, KeyButMask},
};
static X11_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[test]
#[ignore = "requires an explicitly isolated Xvfb DISPLAY"]
fn x11_injection_capture_filter_and_grab_cleanup() {
    let _serial = X11_TEST_LOCK.lock().unwrap();
    let display = std::env::var("DISPLAY").expect("Xvfb DISPLAY");
    assert_eq!(
        std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
        Some(display.as_str()),
        "explicit isolated-display opt in required"
    );
    assert_ne!(display, ":0", "never run input tests on the ordinary desktop");
    let backend = oink_input::platform_backend().unwrap();
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    backend
        .start(Arc::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            oink_input::Disposition::Pass
        }))
        .unwrap();
    backend
        .inject(&InjectEvent::MoveTo {
            pos: Point::new(120., 130.),
        })
        .unwrap();
    assert_eq!(backend.cursor_position().unwrap(), Point::new(120., 130.));
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    backend
        .inject(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        })
        .unwrap();
    assert!(
        conn.query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    backend
        .inject(&InjectEvent::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true,
        })
        .unwrap();
    backend.stop_injection_session();
    assert!(
        !conn
            .query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    assert!(
        backend
            .set_cursor_constraint(Some(oink_foundation::Rect::new(f64::NAN, 0., 1., 1.)))
            .is_err()
    );
    backend
        .set_mode(CaptureMode::Remote {
            park: Point::new(300., 300.),
        })
        .unwrap();
    backend
        .inject(&InjectEvent::MoveTo {
            pos: Point::new(400., 400.),
        })
        .unwrap();
    assert_eq!(backend.cursor_position().unwrap(), Point::new(300., 300.));
    backend.restore_local(Point::new(500., 350.)).unwrap();
    assert_eq!(backend.cursor_position().unwrap(), Point::new(500., 350.));
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        observed.load(Ordering::Relaxed),
        0,
        "own XTest input must not feed routing"
    );
    // A native source drag already owns the pointer grab. Freezing must retain it.
    backend
        .inject(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        })
        .unwrap();
    let source_grab = conn
        .grab_pointer(
            false,
            root,
            x11rb::protocol::xproto::EventMask::BUTTON_PRESS
                | x11rb::protocol::xproto::EventMask::BUTTON_RELEASE
                | x11rb::protocol::xproto::EventMask::POINTER_MOTION,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            0u32,
            0u32,
            0u32,
        )
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(source_grab.status, x11rb::protocol::xproto::GrabStatus::SUCCESS);
    let anchor = backend.freeze_drag_anchor(42).unwrap();
    assert_eq!(anchor.at, Point::new(500., 350.));
    assert_eq!(anchor.surface, 0);
    assert!(
        backend.relocate_drag_anchor(43, Point::new(520., 360.)).is_err(),
        "stale generations must not move a native source drag"
    );
    let posted = std::sync::Mutex::new(None);
    backend
        .move_held_drag_observed(42, Point::new(520., 360.), &|p| *posted.lock().unwrap() = Some(p))
        .unwrap();
    assert_eq!(*posted.lock().unwrap(), Some(Point::new(520., 360.)));
    assert!(
        conn.query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    backend.set_drag_anchor(None).unwrap();
    conn.ungrab_pointer(0u32).unwrap().check().unwrap();
    backend
        .inject(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        })
        .unwrap();
    backend.stop();
    assert_eq!(backend.capture_active(), Some(false));
    // A second client can grab after stop: no native grab was leaked.
    let grabbed = conn
        .grab_keyboard(
            false,
            root,
            0u32,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            x11rb::protocol::xproto::GrabMode::ASYNC,
        )
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(grabbed.status, x11rb::protocol::xproto::GrabStatus::SUCCESS);
    conn.ungrab_keyboard(0u32).unwrap().check().unwrap();
}

#[test]
#[ignore = "requires an explicitly isolated Xvfb DISPLAY"]
fn x11_physical_keyboard_swallow_pass_and_rewrite() {
    use oink_foundation::input::{CapturedEvent, Disposition, InputEvent, LocalRewrite};
    use oink_input::capture_loop::{CaptureLoop, Wake};
    use x11rb::protocol::{
        Event,
        xinput::{self, ConnectionExt as _},
        xkb::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    };
    let _serial = X11_TEST_LOCK.lock().unwrap();
    let display = std::env::var("DISPLAY").unwrap();
    assert_eq!(
        std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
        Some(display.as_str())
    );
    assert_ne!(display, ":0");
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    conn.xinput_xi_query_version(2, 1).unwrap().reply().unwrap();
    let xi_base = conn
        .query_extension(b"XInputExtension")
        .unwrap()
        .reply()
        .unwrap()
        .first_event;
    conn.xkb_use_extension(1, 0).unwrap().reply().unwrap();
    let keyboard = conn
        .xinput_xi_query_device(xinput::Device::ALL)
        .unwrap()
        .reply()
        .unwrap()
        .infos
        .into_iter()
        .find(|d| {
            d.type_ == xinput::DeviceType::SLAVE_KEYBOARD
                && !String::from_utf8_lossy(&d.name).to_lowercase().contains("xtest")
        })
        .expect("isolated Xvfb keyboard");
    let names = conn
        .xkb_get_names(xkb::ID::USE_CORE_KBD.into(), xkb::NameDetail::KEY_NAMES)
        .unwrap()
        .reply()
        .unwrap();
    let first = names.first_key;
    let names = names.value_list.key_names.unwrap();
    let native = |name: [u8; 4]| -> u8 { first + names.iter().position(|k| k.name == name).unwrap() as u8 };
    let a = native(*b"AC01");
    let b = native(*b"AB05");
    let ctrl = native(*b"LCTL");
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        320,
        200,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        0,
        &x11rb::protocol::xproto::CreateWindowAux::default().event_mask(
            x11rb::protocol::xproto::EventMask::KEY_PRESS | x11rb::protocol::xproto::EventMask::KEY_RELEASE,
        ),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    conn.set_input_focus(x11rb::protocol::xproto::InputFocus::PARENT, window, 0u32)
        .unwrap()
        .check()
        .unwrap();
    struct Owner {
        stage: Arc<AtomicUsize>,
        count: Arc<AtomicUsize>,
        pressed: bool,
        rewrite: bool,
    }
    impl CaptureLoop for Owner {
        fn set_wake(&mut self, _: Wake) {}
        fn event(&mut self, e: &CapturedEvent) -> Disposition {
            self.count.fetch_add(1, Ordering::Relaxed);
            if let InputEvent::Key { code, pressed } = e.event {
                self.pressed = pressed;
                self.rewrite = code == KeyCode::keyboard(0x04);
            }
            if self.stage.load(Ordering::Acquire) == 0 {
                Disposition::Swallow
            } else {
                Disposition::Pass
            }
        }
        fn keyboard_disabled(&self) -> bool {
            self.stage.load(Ordering::Acquire) == 3
        }
        fn local_rewrite(&self) -> Option<LocalRewrite> {
            (self.stage.load(Ordering::Acquire) == 2 && self.rewrite).then_some(LocalRewrite::Key {
                code: KeyCode::keyboard(0x05),
                pressed: self.pressed,
            })
        }
        fn poll(&mut self, _: std::time::Instant) -> Option<std::time::Instant> {
            None
        }
        fn stopped(&self) -> bool {
            false
        }
    }
    let stage = Arc::new(AtomicUsize::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let backend = oink_input::platform_backend().unwrap();
    backend
        .start_loop(Box::new(Owner {
            stage: stage.clone(),
            count: count.clone(),
            pressed: false,
            rewrite: false,
        }))
        .unwrap();
    let physical = |code: u8, pressed: bool| {
        conn.xtest_fake_input(
            if pressed {
                xi_base + xinput::DEVICE_KEY_PRESS_EVENT
            } else {
                xi_base + xinput::DEVICE_KEY_RELEASE_EVENT
            },
            code,
            0u32,
            root,
            0,
            0,
            keyboard.deviceid as u8,
        )
        .unwrap()
        .check()
        .unwrap();
    };
    let wait = |expected: usize| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while count.load(Ordering::Acquire) < expected && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            count.load(Ordering::Acquire),
            expected,
            "physical slave events were not captured exactly once"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let drain = || {
        let mut events = Vec::new();
        while let Some(e) = conn.poll_for_event().unwrap() {
            match e {
                Event::KeyPress(e) => events.push((e.detail, true, e.state)),
                Event::KeyRelease(e) => events.push((e.detail, false, e.state)),
                _ => {}
            }
        }
        events
    };
    drain();
    physical(a, true);
    physical(a, false);
    wait(2);
    assert!(drain().is_empty(), "Swallow must not leak to the focused client");
    physical(ctrl, true);
    wait(3);
    conn.xtest_fake_input(2, a, 0u32, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    conn.xtest_fake_input(3, a, 0u32, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let observed = drain();
    assert!(
        observed.iter().all(|(_, _, mods)| !mods.contains(KeyButMask::CONTROL)),
        "swallowed physical Ctrl must not affect master state"
    );
    physical(ctrl, false);
    wait(4);
    drain();
    stage.store(1, Ordering::Release);
    physical(a, true);
    physical(a, false);
    wait(6);
    let observed = drain();
    assert_eq!(
        observed.iter().map(|(k, p, _)| (*k, *p)).collect::<Vec<_>>(),
        vec![(a, true), (a, false)],
        "Pass must reach the focused client once"
    );
    stage.store(2, Ordering::Release);
    physical(a, true);
    physical(a, false);
    wait(8);
    let observed = drain();
    assert_eq!(
        observed.iter().map(|(k, p, _)| (*k, *p)).collect::<Vec<_>>(),
        vec![(b, true), (b, false)],
        "LocalRewrite must not leak the original key"
    );
    stage.store(3, Ordering::Release);
    physical(a, true);
    physical(a, false);
    wait(10);
    assert!(
        drain().is_empty(),
        "keyboard_disabled must enforce swallowing even when the owner passes"
    );
    stage.store(1, Ordering::Release);
    backend.set_keyboard_remote(true).unwrap();
    physical(a, true);
    physical(a, false);
    wait(12);
    assert!(
        drain().is_empty(),
        "remote keyboard mode must not replay physical input locally"
    );
    backend.set_keyboard_remote(false).unwrap();
    backend.stop();
    conn.destroy_window(window).unwrap().check().unwrap();
    let device = conn
        .xinput_xi_query_device(keyboard.deviceid)
        .unwrap()
        .reply()
        .unwrap()
        .infos
        .remove(0);
    assert_eq!(
        device.type_,
        xinput::DeviceType::SLAVE_KEYBOARD,
        "stop must reattach the keyboard to its original master"
    );
}

#[test]
#[ignore = "requires an explicitly isolated Xvfb DISPLAY"]
fn x11_physical_pointer_guard_scroll_motion_and_source_hold() {
    use oink_foundation::input::{CapturedEvent, Disposition, InputEvent, LocalRewrite};
    use oink_input::capture_loop::{CaptureLoop, Wake};
    use x11rb::protocol::{
        Event,
        xinput::{self, ConnectionExt as _},
        xtest::{self, ConnectionExt as _},
    };
    use x11rb::{connection::RequestConnection, x11_utils::Serialize};
    let _serial = X11_TEST_LOCK.lock().unwrap();
    let display = std::env::var("DISPLAY").unwrap();
    assert_eq!(
        std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
        Some(display.as_str())
    );
    assert_ne!(display, ":0");
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let xi_base = conn
        .query_extension(b"XInputExtension")
        .unwrap()
        .reply()
        .unwrap()
        .first_event;
    let xtest_opcode = conn.query_extension(b"XTEST").unwrap().reply().unwrap().major_opcode;
    conn.xinput_xi_query_version(2, 1).unwrap().reply().unwrap();
    let pointer = conn
        .xinput_xi_query_device(xinput::Device::ALL)
        .unwrap()
        .reply()
        .unwrap()
        .infos
        .into_iter()
        .find(|d| {
            d.type_ == xinput::DeviceType::SLAVE_POINTER
                && !String::from_utf8_lossy(&d.name).to_lowercase().contains("xtest")
        })
        .unwrap();
    // Xvfb's physical fixture has 3 buttons. Its native mapping provides wheel
    // buttons without changing any real device or weakening native-event checks.
    assert_eq!(
        u8::from(
            conn.xinput_set_device_button_mapping(pointer.deviceid as u8, &[1, 4, 5])
                .unwrap()
                .reply()
                .unwrap()
                .status
        ),
        0
    );
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        320,
        200,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        0,
        &x11rb::protocol::xproto::CreateWindowAux::default().event_mask(
            x11rb::protocol::xproto::EventMask::BUTTON_PRESS
                | x11rb::protocol::xproto::EventMask::BUTTON_RELEASE
                | x11rb::protocol::xproto::EventMask::POINTER_MOTION,
        ),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    struct Owner {
        stage: Arc<AtomicUsize>,
        count: Arc<AtomicUsize>,
        motion: Arc<std::sync::Mutex<(Point, Point)>>,
        scroll: bool,
    }
    impl CaptureLoop for Owner {
        fn set_wake(&mut self, _: Wake) {}
        fn event(&mut self, e: &CapturedEvent) -> Disposition {
            self.scroll = matches!(e.event, InputEvent::Scroll { .. });
            match e.event {
                InputEvent::PointerDelta { dx, dy } => {
                    self.motion.lock().unwrap().0 = Point::new(dx, dy);
                    self.count.fetch_add(1, Ordering::Release);
                }
                InputEvent::RawPointerDelta { dx, dy } => {
                    self.motion.lock().unwrap().1 = Point::new(dx, dy);
                }
                InputEvent::Button { .. } | InputEvent::Scroll { .. } => {
                    self.count.fetch_add(1, Ordering::Release);
                }
                _ => {}
            }
            Disposition::Pass
        }
        fn mouse_disabled(&self) -> bool {
            self.stage.load(Ordering::Acquire) == 0
        }
        fn local_rewrite(&self) -> Option<LocalRewrite> {
            (self.stage.load(Ordering::Acquire) == 2 && self.scroll).then_some(LocalRewrite::ReverseWheel)
        }
        fn poll(&mut self, _: std::time::Instant) -> Option<std::time::Instant> {
            None
        }
        fn stopped(&self) -> bool {
            false
        }
    }
    let stage = Arc::new(AtomicUsize::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let motion = Arc::new(std::sync::Mutex::new((Point::default(), Point::default())));
    let backend = oink_input::platform_backend().unwrap();
    backend
        .start_loop(Box::new(Owner {
            stage: stage.clone(),
            count: count.clone(),
            motion: motion.clone(),
            scroll: false,
        }))
        .unwrap();
    backend
        .inject(&InjectEvent::MoveTo {
            pos: Point::new(100., 100.),
        })
        .unwrap();
    let physical_button = |button: u8, pressed: bool| {
        conn.xtest_fake_input(
            xi_base
                + if pressed {
                    xinput::DEVICE_BUTTON_PRESS_EVENT
                } else {
                    xinput::DEVICE_BUTTON_RELEASE_EVENT
                },
            button,
            0u32,
            root,
            0,
            0,
            pointer.deviceid as u8,
        )
        .unwrap()
        .check()
        .unwrap();
    };
    let physical_motion = |dx: i32, dy: i32| {
        let (bufs, fds) = xtest::FakeInputRequest {
            type_: xi_base + xinput::DEVICE_MOTION_NOTIFY_EVENT,
            detail: 1,
            time: 0,
            root,
            root_x: 0,
            root_y: 0,
            deviceid: pointer.deviceid as u8,
        }
        .serialize(xtest_opcode);
        let mut bytes = bufs[0].to_vec();
        bytes.extend_from_slice(
            &xinput::DeviceValuatorEvent {
                response_type: xi_base + xinput::DEVICE_VALUATOR_EVENT,
                device_id: pointer.deviceid as u8,
                sequence: 0,
                device_state: 0,
                num_valuators: 2,
                first_valuator: 0,
                valuators: [dx, dy, 0, 0, 0, 0],
            }
            .serialize(),
        );
        let len = (bytes.len() / 4) as u16;
        bytes[2..4].copy_from_slice(&len.to_ne_bytes());
        conn.send_request_without_reply(&[std::io::IoSlice::new(&bytes)], fds)
            .unwrap()
            .check()
            .unwrap();
    };
    let wait = |n: usize| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while count.load(Ordering::Acquire) < n && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(count.load(Ordering::Acquire), n);
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let drain = || {
        let mut buttons = Vec::new();
        while let Some(e) = conn.poll_for_event().unwrap() {
            match e {
                Event::ButtonPress(e) => buttons.push((e.detail, true)),
                Event::ButtonRelease(e) => buttons.push((e.detail, false)),
                _ => {}
            }
        }
        buttons
    };
    drain();
    physical_button(1, true);
    physical_button(1, false);
    wait(2);
    assert!(drain().is_empty(), "disabled source mouse must not leak button events");
    assert!(
        !conn
            .query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    physical_motion(10, 4);
    wait(3);
    assert_eq!(
        backend.cursor_position().unwrap(),
        Point::new(100., 100.),
        "disabled source mouse must not move master"
    );
    stage.store(1, Ordering::Release);
    physical_motion(10, 4);
    wait(4);
    let (accel, raw) = *motion.lock().unwrap();
    assert_eq!(raw, Point::new(10., 4.));
    assert_eq!(
        backend.cursor_position().unwrap(),
        Point::new((100. + accel.x).floor(), (100. + accel.y).floor()),
        "local replay must preserve source acceleration exactly once"
    );
    backend
        .inject(&InjectEvent::MoveTo {
            pos: Point::new(100., 100.),
        })
        .unwrap();
    drain();
    physical_button(1, true);
    physical_button(1, false);
    wait(6);
    assert_eq!(drain(), vec![(1, true), (1, false)]);
    stage.store(2, Ordering::Release);
    physical_button(2, true);
    physical_button(2, false);
    wait(7);
    assert_eq!(
        drain(),
        vec![(5, true), (5, false)],
        "reverse wheel must emit opposite native wheel without original leakage"
    );
    stage.store(1, Ordering::Release);
    physical_button(1, true);
    wait(8);
    drain();
    assert_eq!(
        conn.grab_pointer(
            false,
            root,
            x11rb::protocol::xproto::EventMask::BUTTON_PRESS
                | x11rb::protocol::xproto::EventMask::BUTTON_RELEASE
                | x11rb::protocol::xproto::EventMask::POINTER_MOTION,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            0u32,
            0u32,
            0u32
        )
        .unwrap()
        .reply()
        .unwrap()
        .status,
        x11rb::protocol::xproto::GrabStatus::SUCCESS
    );
    let anchor = backend.freeze_drag_anchor(71).unwrap();
    backend
        .set_mode(CaptureMode::Remote {
            park: Point::new(10., 10.),
        })
        .unwrap();
    physical_button(1, false);
    wait(9);
    assert!(
        conn.query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1),
        "source hold must retain LEFT after physical release"
    );
    backend
        .move_held_drag_observed(71, Point::new(80., 80.), &|_| {})
        .unwrap();
    backend.set_drag_anchor(None).unwrap();
    assert_eq!(backend.cursor_position().unwrap(), Point::new(80., 80.));
    assert!(
        conn.query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    backend.release_physical_drag().unwrap();
    assert!(
        !conn
            .query_pointer(root)
            .unwrap()
            .reply()
            .unwrap()
            .mask
            .contains(KeyButMask::BUTTON1)
    );
    conn.ungrab_pointer(0u32).unwrap().check().unwrap();
    let _ = anchor;
    // Pause withdraws grabs, so the OS gets physical input directly. Resume
    // reattaches interception exactly once without replaying queued stale input.
    backend.set_input_paused(true);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while conn
        .xinput_xi_query_device(pointer.deviceid)
        .unwrap()
        .reply()
        .unwrap()
        .infos[0]
        .type_
        != xinput::DeviceType::SLAVE_POINTER
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    drain();
    physical_button(1, true);
    physical_button(1, false);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(count.load(Ordering::Acquire), 9, "paused source must not be routed");
    assert_eq!(
        drain(),
        vec![(1, true), (1, false)],
        "pause must return ordinary input to the OS"
    );
    backend.set_input_paused(false);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while conn
        .xinput_xi_query_device(pointer.deviceid)
        .unwrap()
        .reply()
        .unwrap()
        .infos[0]
        .type_
        != xinput::DeviceType::FLOATING_SLAVE
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    stage.store(0, Ordering::Release);
    drain();
    physical_button(1, true);
    physical_button(1, false);
    wait(11);
    assert!(drain().is_empty(), "resumed disabled source must be intercepted again");
    backend.stop();
    assert_eq!(
        conn.xinput_xi_query_device(pointer.deviceid)
            .unwrap()
            .reply()
            .unwrap()
            .infos[0]
            .type_,
        xinput::DeviceType::SLAVE_POINTER
    );
    conn.destroy_window(window).unwrap().check().unwrap();
}

#[test]
#[ignore = "requires an explicitly isolated Xvfb DISPLAY"]
fn x11_real_router_linux_mapping_alias_and_reverse_wheel() {
    use oink_foundation::{
        DeviceId,
        input::{CapturedEvent, Disposition, LocalRewrite},
        input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping},
        session::Boot,
    };
    use oink_input::capture_loop::{CaptureLoop, Wake};
    use x11rb::protocol::{
        Event,
        xinput::{self, ConnectionExt as _},
        xkb::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    };
    let _serial = X11_TEST_LOCK.lock().unwrap();
    let display = std::env::var("DISPLAY").unwrap();
    assert_eq!(
        std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
        Some(display.as_str())
    );
    assert_ne!(display, ":0");
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    conn.xinput_xi_query_version(2, 1).unwrap().reply().unwrap();
    conn.xkb_use_extension(1, 0).unwrap().reply().unwrap();
    let xi = conn
        .query_extension(b"XInputExtension")
        .unwrap()
        .reply()
        .unwrap()
        .first_event;
    let devices = conn
        .xinput_xi_query_device(xinput::Device::ALL)
        .unwrap()
        .reply()
        .unwrap()
        .infos;
    let keyboard = devices
        .iter()
        .find(|d| {
            d.type_ == xinput::DeviceType::SLAVE_KEYBOARD
                && !String::from_utf8_lossy(&d.name).to_lowercase().contains("xtest")
        })
        .unwrap();
    let pointer = devices
        .iter()
        .find(|d| {
            d.type_ == xinput::DeviceType::SLAVE_POINTER
                && !String::from_utf8_lossy(&d.name).to_lowercase().contains("xtest")
        })
        .unwrap();
    assert_eq!(
        u8::from(
            conn.xinput_set_device_button_mapping(pointer.deviceid as u8, &[1, 4, 5])
                .unwrap()
                .reply()
                .unwrap()
                .status
        ),
        0
    );
    let names = conn
        .xkb_get_names(xkb::ID::USE_CORE_KBD.into(), xkb::NameDetail::KEY_NAMES)
        .unwrap()
        .reply()
        .unwrap();
    let first = names.first_key;
    let names = names.value_list.key_names.unwrap();
    let native = |name: [u8; 4]| first + names.iter().position(|k| k.name == name).unwrap() as u8;
    let (a, b) = (native(*b"AC01"), native(*b"AB05"));
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        320,
        200,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        0,
        &x11rb::protocol::xproto::CreateWindowAux::default().event_mask(
            x11rb::protocol::xproto::EventMask::KEY_PRESS
                | x11rb::protocol::xproto::EventMask::KEY_RELEASE
                | x11rb::protocol::xproto::EventMask::BUTTON_PRESS
                | x11rb::protocol::xproto::EventMask::BUTTON_RELEASE,
        ),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    conn.set_input_focus(x11rb::protocol::xproto::InputFocus::PARENT, window, 0u32)
        .unwrap()
        .check()
        .unwrap();
    let id = DeviceId([49; 32]);
    let mut router = oink_routing::Router::new(
        oink_routing::RouterConfig::new(
            Boot {
                device_id: id,
                nonce: [49; 16],
            },
            "linux",
            oink_routing::Os::Linux,
        ),
        Arc::new(oink_routing::SimpleLayout::new()),
    );
    let mut transform = InputTransform::default();
    transform.linux.reverse_wheel = true;
    transform.linux.keyboard = KeyboardMapping {
        preset: KeyPreset::Custom,
        table: vec![KeyMapping {
            from: KeyCode::keyboard(4),
            to: KeyCode::keyboard(5),
        }],
        ..Default::default()
    };
    router.set_input_transforms([(id, transform)].into());
    struct Owner {
        router: oink_routing::Router,
        count: Arc<AtomicUsize>,
    }
    impl CaptureLoop for Owner {
        fn set_wake(&mut self, _: Wake) {}
        fn event(&mut self, e: &CapturedEvent) -> Disposition {
            let result = self.router.on_local_event(e, e.time_us);
            self.count.fetch_add(1, Ordering::Release);
            result
        }
        fn local_rewrite(&self) -> Option<LocalRewrite> {
            self.router.local_rewrite()
        }
        fn poll(&mut self, _: std::time::Instant) -> Option<std::time::Instant> {
            None
        }
        fn stopped(&self) -> bool {
            false
        }
    }
    let count = Arc::new(AtomicUsize::new(0));
    let backend = oink_input::platform_backend().unwrap();
    backend
        .start_loop(Box::new(Owner {
            router,
            count: count.clone(),
        }))
        .unwrap();
    backend
        .inject(&InjectEvent::MoveTo {
            pos: Point::new(100., 100.),
        })
        .unwrap();
    while conn.poll_for_event().unwrap().is_some() {}
    let physical = |code: u8, down: bool| {
        conn.xtest_fake_input(
            xi + if down {
                xinput::DEVICE_KEY_PRESS_EVENT
            } else {
                xinput::DEVICE_KEY_RELEASE_EVENT
            },
            code,
            0u32,
            root,
            0,
            0,
            keyboard.deviceid as u8,
        )
        .unwrap()
        .check()
        .unwrap();
    };
    physical(a, true);
    physical(b, true);
    physical(a, false);
    physical(b, false);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while count.load(Ordering::Acquire) < 4 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(count.load(Ordering::Acquire), 4);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut keys = Vec::new();
    while let Some(e) = conn.poll_for_event().unwrap() {
        match e {
            Event::KeyPress(e) => keys.push((e.detail, true)),
            Event::KeyRelease(e) => keys.push((e.detail, false)),
            _ => {}
        }
    }
    assert_eq!(
        keys,
        vec![(b, true), (b, false)],
        "real router and XI2 alias leases must expose one mapped native pair"
    );
    for down in [true, false] {
        conn.xtest_fake_input(
            xi + if down {
                xinput::DEVICE_BUTTON_PRESS_EVENT
            } else {
                xinput::DEVICE_BUTTON_RELEASE_EVENT
            },
            2,
            0u32,
            root,
            0,
            0,
            pointer.deviceid as u8,
        )
        .unwrap()
        .check()
        .unwrap();
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while count.load(Ordering::Acquire) < 5 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(count.load(Ordering::Acquire), 5);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut wheel = Vec::new();
    while let Some(e) = conn.poll_for_event().unwrap() {
        match e {
            Event::ButtonPress(e) => wheel.push((e.detail, true)),
            Event::ButtonRelease(e) => wheel.push((e.detail, false)),
            _ => {}
        }
    }
    assert_eq!(
        wheel,
        vec![(5, true), (5, false)],
        "real Linux router must reach the native reverse-wheel implementation"
    );
    backend.stop();
    conn.destroy_window(window).unwrap().check().unwrap();
}
