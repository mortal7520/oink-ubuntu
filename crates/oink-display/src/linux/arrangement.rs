//! Session-only RandR origin writes. The server grab serializes other clients;
//! validation precedes every write and failed partial writes restore the saved
//! framebuffer and CRTCs. No mode, rotation, primary or output membership changes.
use super::{X11Display, configuration, error};
use crate::{
    DisplayError,
    arrangement::{ArrangementError, NativeConfiguration, NativeOrigin},
};
#[cfg(test)]
use oink_foundation::{Point, Rect};
use std::collections::{BTreeMap, BTreeSet};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{
        randr::{self, ConnectionExt as _},
        xproto::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    width: u16,
    height: u16,
    mm_width: u32,
    mm_height: u32,
}
impl Frame {
    fn resized(self, width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            mm_width: ((f64::from(self.mm_width) * f64::from(width) / f64::from(self.width)).round() as u32).max(1),
            mm_height: ((f64::from(self.mm_height) * f64::from(height) / f64::from(self.height)).round() as u32).max(1),
        }
    }
}
#[derive(Clone)]
struct Crtc {
    id: u32,
    x: i16,
    y: i16,
    mode: u32,
    rotation: randr::Rotation,
    outputs: Vec<u32>,
}
struct Snapshot {
    configuration: NativeConfiguration,
    frame: Frame,
    crtcs: Vec<Crtc>,
    handles: BTreeMap<String, u32>,
    config_time: u32,
    range: (u16, u16, u16, u16),
}
struct Plan {
    configuration: NativeConfiguration,
    frame: Frame,
    positions: BTreeMap<u32, (i16, i16)>,
}
struct ServerGrab<'a>(&'a RustConnection);
impl Drop for ServerGrab<'_> {
    fn drop(&mut self) {
        let _ = self.0.ungrab_server();
        let _ = self.0.flush();
    }
}
fn same_topology(a: &NativeConfiguration, b: &NativeConfiguration) -> bool {
    a.displays.len() == b.displays.len()
        && a.displays.iter().zip(&b.displays).all(|(a, b)| {
            a.key == b.key
                && a.mode == b.mode
                && a.primary == b.primary
                && a.mirror_of == b.mirror_of
                && a.bounds.w == b.bounds.w
                && a.bounds.h == b.bounds.h
        })
}
fn identity(t: x11rb::protocol::render::Transform) -> bool {
    [
        t.matrix11, t.matrix12, t.matrix13, t.matrix21, t.matrix22, t.matrix23, t.matrix31, t.matrix32, t.matrix33,
    ] == [65536, 0, 0, 0, 65536, 0, 0, 0, 65536]
}
impl Snapshot {
    fn read(backend: &X11Display) -> Result<Self, DisplayError> {
        let conn = &backend.connection;
        let root = conn.setup().roots[backend.screen].root;
        let resources = conn
            .randr_get_screen_resources_current(root)
            .map_err(error)?
            .reply()
            .map_err(|e| error(format!("randr_get_screen_resources_current(root): {e}")))?;
        let outputs = backend.outputs()?;
        let handles: BTreeMap<_, _> = outputs.iter().map(|o| (o.info.key.clone(), o.crtc)).collect();
        let configuration = configuration(outputs)?;
        let mut crtcs = Vec::new();
        for id in resources.crtcs {
            let c = conn
                .randr_get_crtc_info(id, resources.config_timestamp)
                .map_err(error)?
                .reply()
                .map_err(error)?;
            if c.status != randr::SetConfig::SUCCESS {
                return Err(ArrangementError::Changed.into());
            }
            if c.mode != 0 {
                crtcs.push(Crtc {
                    id,
                    x: c.x,
                    y: c.y,
                    mode: c.mode,
                    rotation: c.rotation,
                    outputs: c.outputs,
                });
            }
        }
        let active: BTreeSet<_> = crtcs.iter().map(|c| c.id).collect();
        let represented: BTreeSet<_> = configuration
            .displays
            .iter()
            .filter_map(|d| handles.get(&d.key).copied())
            .collect();
        if active != represented {
            return Err(ArrangementError::Changed.into());
        }
        let g = conn.get_geometry(root).map_err(error)?.reply().map_err(error)?;
        let setup = &conn.setup().roots[backend.screen];
        // apply opens a fresh native connection before grabbing the server. Its core
        // setup contains the real framebuffer millimetres (legacy RandR screen-info
        // can instead return the first output's physical size). Reject a resize race.
        if (g.width, g.height) != (setup.width_in_pixels, setup.height_in_pixels) {
            return Err(ArrangementError::Changed.into());
        }
        if setup.width_in_millimeters == 0 || setup.height_in_millimeters == 0 {
            return Err(error(
                "framebuffer physical size is unavailable; cannot preserve DPI during an origin-only transaction",
            ));
        }
        let frame = Frame {
            width: g.width,
            height: g.height,
            mm_width: u32::from(setup.width_in_millimeters),
            mm_height: u32::from(setup.height_in_millimeters),
        };
        let range = conn
            .randr_get_screen_size_range(root)
            .map_err(error)?
            .reply()
            .map_err(|e| error(format!("randr_get_screen_size_range(root): {e}")))?;
        Ok(Self {
            configuration,
            frame,
            crtcs,
            handles,
            config_time: resources.config_timestamp,
            range: (range.min_width, range.min_height, range.max_width, range.max_height),
        })
    }
    fn ensure_supported(&self, conn: &RustConnection) -> Result<(), DisplayError> {
        // Moving a transformed/panning CRTC also changes its viewport/panning extent.
        // An origin-only transaction must not reset or silently reinterpret either.
        for c in &self.crtcs {
            let t = conn
                .randr_get_crtc_transform(c.id)
                .map_err(error)?
                .reply()
                .map_err(|e| error(format!("randr_get_crtc_transform(c.id): {e}")))?;
            let p = conn
                .randr_get_panning(c.id)
                .map_err(error)?
                .reply()
                .map_err(|e| error(format!("randr_get_panning(c.id): {e}")))?;
            if p.status != randr::SetConfig::SUCCESS
                || !identity(t.current_transform)
                || !identity(t.pending_transform)
                || p.width != 0
                || p.height != 0
            {
                return Err(error(
                    "origin-only writes for transformed or panning CRTCs are unsupported; use the system display settings",
                ));
            }
        }
        Ok(())
    }
    fn plan(&self, origins: &[NativeOrigin]) -> Result<Plan, DisplayError> {
        let mut next = self.configuration.with_origins(origins)?;
        // The shared plan is primary-relative. RandR's root is always nonnegative;
        // translate the entire layout, then return actual root coordinates to callers.
        let min_x = next.sources().map(|d| d.bounds.x).fold(0., f64::min);
        let min_y = next.sources().map(|d| d.bounds.y).fold(0., f64::min);
        let mut positions = BTreeMap::new();
        let (mut width, mut height) = (0f64, 0f64);
        for d in &mut next.displays {
            d.bounds.x -= min_x;
            d.bounds.y -= min_y;
            if d.bounds.x > f64::from(i16::MAX) || d.bounds.y > f64::from(i16::MAX) {
                return Err(ArrangementError::Invalid.into());
            }
            width = width.max(d.bounds.x + d.bounds.w);
            height = height.max(d.bounds.y + d.bounds.h);
            let crtc = *self.handles.get(&d.key).ok_or(ArrangementError::Changed)?;
            let at = (d.bounds.x as i16, d.bounds.y as i16);
            if positions.insert(crtc, at).is_some_and(|old| old != at) {
                return Err(ArrangementError::Invalid.into());
            }
        }
        let (min_w, min_h, max_w, max_h) = self.range;
        width = width.max(f64::from(min_w));
        height = height.max(f64::from(min_h));
        if width > f64::from(max_w) || height > f64::from(max_h) {
            return Err(error("arrangement exceeds the X11 framebuffer size range"));
        }
        Ok(Plan {
            configuration: next,
            frame: self.frame.resized(width as u16, height as u16),
            positions,
        })
    }
}
trait TransactionApi {
    fn frame(&mut self, frame: Frame) -> Result<(), DisplayError>;
    fn crtc(&mut self, crtc: &Crtc, x: i16, y: i16) -> Result<(), DisplayError>;
    fn observe(&mut self) -> Result<NativeConfiguration, DisplayError>;
    fn rollback_allowed(&mut self, expected: &NativeConfiguration) -> Result<(), DisplayError> {
        if same_topology(&self.observe()?, expected) {
            Ok(())
        } else {
            Err(ArrangementError::Changed.into())
        }
    }
}
struct RandrApi<'a> {
    backend: &'a X11Display,
    config_time: u32,
}
impl TransactionApi for RandrApi<'_> {
    fn frame(&mut self, frame: Frame) -> Result<(), DisplayError> {
        let conn = &self.backend.connection;
        let root = conn.setup().roots[self.backend.screen].root;
        conn.randr_set_screen_size(root, frame.width, frame.height, frame.mm_width, frame.mm_height)
            .map_err(error)?
            .check()
            .map_err(error)
    }
    fn crtc(&mut self, c: &Crtc, x: i16, y: i16) -> Result<(), DisplayError> {
        let r = self
            .backend
            .connection
            .randr_set_crtc_config(
                c.id,
                CURRENT_TIME,
                self.config_time,
                x,
                y,
                c.mode,
                c.rotation,
                &c.outputs,
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        if r.status != randr::SetConfig::SUCCESS {
            return Err(error(format!("RandR CRTC {} origin rejected: {:?}", c.id, r.status)));
        }
        Ok(())
    }
    fn observe(&mut self) -> Result<NativeConfiguration, DisplayError> {
        configuration(self.backend.outputs()?)
    }
}
fn execute(api: &mut impl TransactionApi, before: &Snapshot, plan: &Plan) -> Result<NativeConfiguration, DisplayError> {
    if plan.configuration == before.configuration {
        return Ok(before.configuration.clone());
    }
    let expanded = before.frame.resized(
        before.frame.width.max(plan.frame.width),
        before.frame.height.max(plan.frame.height),
    );
    let applied: Result<NativeConfiguration, DisplayError> = (|| {
        if expanded != before.frame {
            api.frame(expanded)?;
        }
        for c in &before.crtcs {
            let (x, y) = *plan.positions.get(&c.id).ok_or(ArrangementError::Changed)?;
            if (x, y) != (c.x, c.y) {
                api.crtc(c, x, y)?;
            }
        }
        if plan.frame != expanded {
            api.frame(plan.frame)?;
        }
        let observed = api.observe()?;
        if observed != plan.configuration {
            return Err(ArrangementError::Changed.into());
        }
        Ok(observed)
    })();
    match applied {
        Ok(observed) => Ok(observed),
        Err(cause) => {
            // Preserve unrelated hotplug/topology changes instead of restoring a stale
            // mode/output id onto a newly attached device. Report incomplete restoration.
            let restored = (|| {
                api.rollback_allowed(&before.configuration)?;
                api.frame(expanded)?;
                for c in &before.crtcs {
                    api.crtc(c, c.x, c.y)?;
                }
                api.frame(before.frame)?;
                if api.observe()? != before.configuration {
                    return Err(ArrangementError::Changed.into());
                }
                Ok::<_, DisplayError>(())
            })();
            match restored {
                Ok(()) => Err(error(format!("arrangement failed; previous layout restored: {cause}"))),
                Err(rollback) => Err(error(format!(
                    "arrangement failed ({cause}); layout rollback could not complete ({rollback}); inspect system display settings"
                ))),
            }
        }
    }
}
pub(super) fn apply(
    _backend: &X11Display,
    expected: &NativeConfiguration,
    origins: &[NativeOrigin],
) -> Result<NativeConfiguration, DisplayError> {
    let (connection, screen) = super::connect()?;
    let fresh = X11Display {
        connection,
        screen,
        watches: std::sync::Mutex::new(Vec::new()),
    };
    let backend = &fresh;
    backend
        .connection
        .grab_server()
        .map_err(error)?
        .check()
        .map_err(error)?;
    let _grab = ServerGrab(&backend.connection);
    let before = Snapshot::read(backend)?;
    if &before.configuration != expected {
        return Err(ArrangementError::Changed.into());
    }
    let plan = before.plan(origins)?;
    if plan.configuration == before.configuration {
        return Ok(before.configuration);
    }
    before.ensure_supported(&backend.connection)?;
    execute(
        &mut RandrApi {
            backend,
            config_time: before.config_time,
        },
        &before,
        &plan,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::NativeDisplay;
    fn snapshot() -> Snapshot {
        Snapshot {
            configuration: NativeConfiguration {
                displays: vec![
                    NativeDisplay {
                        key: "a".into(),
                        bounds: Rect::new(0., 0., 800., 600.),
                        primary: true,
                        mirror_of: None,
                        mode: "1".into(),
                    },
                    NativeDisplay {
                        key: "b".into(),
                        bounds: Rect::new(800., 0., 800., 600.),
                        primary: false,
                        mirror_of: None,
                        mode: "1".into(),
                    },
                ],
            },
            frame: Frame {
                width: 1600,
                height: 600,
                mm_width: 400,
                mm_height: 150,
            },
            crtcs: vec![
                Crtc {
                    id: 1,
                    x: 0,
                    y: 0,
                    mode: 1,
                    rotation: randr::Rotation::ROTATE0,
                    outputs: vec![10],
                },
                Crtc {
                    id: 2,
                    x: 800,
                    y: 0,
                    mode: 1,
                    rotation: randr::Rotation::ROTATE0,
                    outputs: vec![20],
                },
            ],
            handles: BTreeMap::from([("a".into(), 1), ("b".into(), 2)]),
            config_time: 1,
            range: (1, 1, 4096, 4096),
        }
    }
    struct Fake {
        current: NativeConfiguration,
        calls: Vec<String>,
        fail: Option<usize>,
        observations: usize,
    }
    impl TransactionApi for Fake {
        fn frame(&mut self, f: Frame) -> Result<(), DisplayError> {
            self.calls.push(format!("frame:{}x{}", f.width, f.height));
            Ok(())
        }
        fn crtc(&mut self, c: &Crtc, x: i16, y: i16) -> Result<(), DisplayError> {
            self.calls.push(format!("crtc:{}:{x},{y}", c.id));
            if self.fail == Some(c.id as usize) {
                self.fail = None;
                return Err(error("fixture CRTC failure"));
            }
            let key = if c.id == 1 { "a" } else { "b" };
            let d = self.current.displays.iter_mut().find(|d| d.key == key).unwrap();
            d.bounds.x = f64::from(x);
            d.bounds.y = f64::from(y);
            Ok(())
        }
        fn observe(&mut self) -> Result<NativeConfiguration, DisplayError> {
            self.observations += 1;
            Ok(self.current.clone())
        }
    }
    #[test]
    fn negative_relative_origins_are_translated_to_actual_root_coordinates() {
        let b = snapshot();
        let p = b
            .plan(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: "b".into(),
                    origin: Point::new(-800., 0.),
                },
            ])
            .unwrap();
        assert_eq!(p.positions.get(&1), Some(&(800, 0)));
        assert_eq!(p.positions.get(&2), Some(&(0, 0)));
        assert_eq!((p.frame.width, p.frame.height), (1600, 600));
    }
    #[test]
    fn expansion_precedes_crtc_and_failure_restores_old_frame_and_origins() {
        let b = snapshot();
        let p = b
            .plan(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: "b".into(),
                    origin: Point::new(0., 600.),
                },
            ])
            .unwrap();
        let mut api = Fake {
            current: b.configuration.clone(),
            calls: Vec::new(),
            fail: Some(2),
            observations: 0,
        };
        assert!(execute(&mut api, &b, &p).is_err());
        assert_eq!(api.current, b.configuration);
        assert_eq!(
            api.calls,
            vec![
                "frame:1600x1200",
                "crtc:2:0,600",
                "frame:1600x1200",
                "crtc:1:0,0",
                "crtc:2:800,0",
                "frame:1600x600"
            ]
        );
    }
    #[test]
    fn shrink_follows_all_crtcs_and_returns_actual_observation() {
        let b = snapshot();
        let p = b
            .plan(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: "b".into(),
                    origin: Point::new(0., 600.),
                },
            ])
            .unwrap();
        let mut api = Fake {
            current: b.configuration.clone(),
            calls: Vec::new(),
            fail: None,
            observations: 0,
        };
        assert_eq!(execute(&mut api, &b, &p).unwrap(), p.configuration);
        assert_eq!(api.calls, vec!["frame:1600x1200", "crtc:2:0,600", "frame:800x1200"]);
    }
    #[test]
    fn unsupported_framebuffer_or_stale_sources_are_rejected_before_execution() {
        let b = snapshot();
        assert!(
            b.plan(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default()
                },
                NativeOrigin {
                    key: "b".into(),
                    origin: Point::new(800., 5000.)
                }
            ])
            .is_err()
        );
        assert!(
            b.plan(&[NativeOrigin {
                key: "a".into(),
                origin: Point::default()
            }])
            .is_err()
        );
    }
    #[test]
    fn rollback_does_not_overwrite_a_foreign_topology() {
        let b = snapshot();
        let p = b
            .plan(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: "b".into(),
                    origin: Point::new(0., 600.),
                },
            ])
            .unwrap();
        let mut foreign = b.configuration.clone();
        foreign.displays[0].mode = "externally changed".into();
        let mut api = Fake {
            current: foreign,
            calls: Vec::new(),
            fail: Some(2),
            observations: 0,
        };
        let e = execute(&mut api, &b, &p).unwrap_err();
        assert!(e.to_string().contains("rollback could not complete"));
        assert_eq!(api.calls, vec!["frame:1600x1200", "crtc:2:0,600"]);
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::DisplayBackend;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[test]
    #[ignore = "requires explicitly isolated dual-CRTC dummy Xorg"]
    fn x11_randr_native_origins_stale_fence_and_failed_write_rollback() {
        let display = std::env::var("DISPLAY").unwrap();
        assert_eq!(
            std::env::var("OINK_X11_TEST_DISPLAY").ok().as_deref(),
            Some(display.as_str())
        );
        assert_eq!(std::env::var("OINK_X11_DUMMY").ok().as_deref(), Some("1"));
        assert_ne!(display, ":0");
        let backend = super::super::backend().unwrap();
        let before = backend.native_configuration().unwrap();
        assert_eq!(before.sources().count(), 2);
        let primary = before.sources().find(|d| d.primary).unwrap();
        let secondary = before.sources().find(|d| !d.primary).unwrap();
        let (a, b) = (primary.key.clone(), secondary.key.clone());
        let (w, h) = (primary.bounds.w, primary.bounds.h);
        let events = Arc::new(AtomicUsize::new(0));
        let count = events.clone();
        backend
            .watch(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();
        let origins = |at: Point| {
            vec![
                NativeOrigin {
                    key: a.clone(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: b.clone(),
                    origin: at,
                },
            ]
        };
        let vertical = backend
            .apply_native_origins(&before, &origins(Point::new(0., h)))
            .unwrap();
        assert!(same_topology(&before, &vertical));
        assert_eq!(vertical.sources().find(|d| d.key == b).unwrap().bounds.y, h);
        assert!(matches!(
            backend.apply_native_origins(&before, &origins(Point::new(w, 0.))),
            Err(DisplayError::Arrangement(ArrangementError::Changed))
        ));
        assert_eq!(backend.native_configuration().unwrap(), vertical);
        let left = backend
            .apply_native_origins(&vertical, &origins(Point::new(-w, 0.)))
            .unwrap();
        assert_eq!(left.sources().find(|d| d.key == a).unwrap().bounds.x, w);
        assert_eq!(left.sources().find(|d| d.key == b).unwrap().bounds.x, 0.);
        let restored = backend
            .apply_native_origins(&left, &origins(Point::new(w, 0.)))
            .unwrap();
        assert_eq!(restored, before);
        // Exercise a real partial CRTC write, then fail the next request. The same
        // native writer must restore actual server state, not just a mocked ledger.
        let (conn, screen) = super::super::connect().unwrap();
        let native = X11Display {
            connection: conn,
            screen,
            watches: std::sync::Mutex::new(Vec::new()),
        };
        native.connection.grab_server().unwrap().check().unwrap();
        let _grab = ServerGrab(&native.connection);
        let saved = Snapshot::read(&native).unwrap();
        let plan = saved.plan(&origins(Point::new(-w, 0.))).unwrap();
        saved.ensure_supported(&native.connection).unwrap();
        struct FailSecond<'a> {
            inner: RandrApi<'a>,
            requests: usize,
            failed: bool,
        }
        impl TransactionApi for FailSecond<'_> {
            fn frame(&mut self, f: Frame) -> Result<(), DisplayError> {
                self.inner.frame(f)
            }
            fn observe(&mut self) -> Result<NativeConfiguration, DisplayError> {
                self.inner.observe()
            }
            fn crtc(&mut self, c: &Crtc, x: i16, y: i16) -> Result<(), DisplayError> {
                self.requests += 1;
                if self.requests == 2 && !self.failed {
                    self.failed = true;
                    return Err(error("injected second-CRTC failure"));
                }
                self.inner.crtc(c, x, y)
            }
        }
        let mut api = FailSecond {
            inner: RandrApi {
                backend: &native,
                config_time: saved.config_time,
            },
            requests: 0,
            failed: false,
        };
        let failed = execute(&mut api, &saved, &plan).unwrap_err();
        assert!(
            api.failed,
            "the injected failure must follow a successful native CRTC write"
        );
        assert!(failed.to_string().contains("previous layout restored"));
        assert_eq!(native.native_configuration().unwrap(), before);
        drop(_grab);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while events.load(Ordering::Acquire) == 0 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            events.load(Ordering::Acquire) > 0,
            "real RandR writes must notify the watcher"
        );
    }
}
