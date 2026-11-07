//! Native X11/RandR backend. XWayland is intentionally not advertised as a
//! compositor-wide backend: its coordinates and input authority are incomplete.
use crate::{ChangeHandler, DisplayBackend, DisplayError, DisplayInfo, arrangement};
#[path = "linux/arrangement.rs"]
mod native_arrangement;
use oink_foundation::{Rect, Size};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::{
    Event,
    randr::{self, ConnectionExt as _},
    xproto::{AtomEnum, ConnectionExt as _},
};
use x11rb::rust_connection::RustConnection;

fn error(e: impl std::fmt::Display) -> DisplayError {
    DisplayError::Platform(format!("X11: {e}"))
}
fn session_supported(session: Option<&str>, wayland: bool) -> bool {
    !session.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
        && !(wayland && !session.is_some_and(|s| s.eq_ignore_ascii_case("x11")))
}
fn connect() -> Result<(RustConnection, usize), DisplayError> {
    if !session_supported(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
    ) {
        return Err(error(
            "Wayland display enumeration is unsupported; log in to an X11 session",
        ));
    }
    let (conn, screen) = x11rb::connect(None).map_err(error)?;
    let version = conn.randr_query_version(1, 3).map_err(error)?.reply().map_err(error)?;
    if (version.major_version, version.minor_version) < (1, 3) {
        return Err(error("RandR 1.3 or newer is required"));
    }
    Ok((conn, screen))
}

struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            if t.thread().id() != thread::current().id() {
                let _ = t.join();
            }
        }
    }
}
struct X11Display {
    connection: RustConnection,
    screen: usize,
    watches: Mutex<Vec<Watch>>,
}
pub(super) fn backend() -> Result<Box<dyn DisplayBackend>, DisplayError> {
    let (connection, screen) = connect()?;
    Ok(Box::new(X11Display {
        connection,
        screen,
        watches: Mutex::new(Vec::new()),
    }))
}
fn edid_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn rotation_degrees(rotation: randr::Rotation) -> u16 {
    if rotation.contains(randr::Rotation::ROTATE90) {
        90
    } else if rotation.contains(randr::Rotation::ROTATE180) {
        180
    } else if rotation.contains(randr::Rotation::ROTATE270) {
        270
    } else {
        0
    }
}
#[derive(Clone)]
struct Output {
    info: DisplayInfo,
    crtc: u32,
    mode: String,
}
impl X11Display {
    fn outputs(&self) -> Result<Vec<Output>, DisplayError> {
        let root = self.connection.setup().roots[self.screen].root;
        let resources = self
            .connection
            .randr_get_screen_resources_current(root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let primary = self
            .connection
            .randr_get_output_primary(root)
            .map_err(error)?
            .reply()
            .map_err(error)?
            .output;
        let edid = self
            .connection
            .intern_atom(true, b"EDID")
            .map_err(error)?
            .reply()
            .map_err(error)?
            .atom;
        let mut outputs = Vec::new();
        for id in resources.outputs {
            let output = self
                .connection
                .randr_get_output_info(id, resources.config_timestamp)
                .map_err(error)?
                .reply()
                .map_err(error)?;
            // Active CRTCs are authoritative: software outputs can remain marked
            // disconnected until the next driver probe after being enabled.
            if output.crtc == 0 {
                continue;
            }
            if output.status != randr::SetConfig::SUCCESS {
                return Err(arrangement::ArrangementError::Changed.into());
            }
            let crtc = self
                .connection
                .randr_get_crtc_info(output.crtc, resources.config_timestamp)
                .map_err(error)?
                .reply()
                .map_err(error)?;
            if crtc.status != randr::SetConfig::SUCCESS {
                return Err(arrangement::ArrangementError::Changed.into());
            }
            if crtc.mode == 0 || crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            let name = String::from_utf8_lossy(&output.name).into_owned();
            let bytes = if edid != 0 {
                self.connection
                    .randr_get_output_property(id, edid, AtomEnum::ANY, 0, 256, false, false)
                    .map_err(error)?
                    .reply()
                    .map_err(error)?
                    .data
            } else {
                Vec::new()
            };
            let key = if bytes.is_empty() {
                format!("x11:{name}")
            } else {
                format!("x11:{name}:{:016x}", edid_hash(&bytes))
            };
            let rotation_deg = rotation_degrees(crtc.rotation);
            let (mw, mh) = if rotation_deg == 90 || rotation_deg == 270 {
                (output.mm_height, output.mm_width)
            } else {
                (output.mm_width, output.mm_height)
            };
            let reference_extent = (mw > 0 && mh > 0).then(|| Size::new(f64::from(mw) / 686.0, f64::from(mh) / 686.0));
            outputs.push(Output {
                crtc: output.crtc,
                mode: format!(
                    "{}:{}:{}:{}",
                    crtc.mode,
                    u16::from(crtc.rotation),
                    crtc.width,
                    crtc.height
                ),
                info: DisplayInfo {
                    key,
                    name: name.clone(),
                    bounds: Rect::new(
                        f64::from(crtc.x),
                        f64::from(crtc.y),
                        f64::from(crtc.width),
                        f64::from(crtc.height),
                    ),
                    pixel_width: u32::from(crtc.width),
                    pixel_height: u32::from(crtc.height),
                    reference_extent,
                    rotation_deg,
                    is_primary: id == primary,
                    is_builtin: name.starts_with("eDP") || name.starts_with("LVDS") || name.starts_with("DSI"),
                    is_virtual: name.starts_with("VIRTUAL") || name.starts_with("Virtual") || name.starts_with("DUMMY"),
                },
            });
        }
        if outputs.is_empty() {
            return Err(error("RandR reports no active outputs"));
        }
        outputs.sort_by(|a, b| {
            b.info
                .is_primary
                .cmp(&a.info.is_primary)
                .then_with(|| a.info.key.cmp(&b.info.key))
        });
        if !outputs.iter().any(|o| o.info.is_primary) {
            outputs[0].info.is_primary = true;
        }
        Ok(outputs)
    }
}
fn configuration(outputs: Vec<Output>) -> Result<arrangement::NativeConfiguration, DisplayError> {
    let mut sources = std::collections::BTreeMap::new();
    let mut displays = Vec::new();
    for o in outputs {
        let mirror_of = sources.get(&o.crtc).cloned();
        sources.entry(o.crtc).or_insert_with(|| o.info.key.clone());
        displays.push(arrangement::NativeDisplay {
            key: o.info.key,
            bounds: o.info.bounds,
            primary: o.info.is_primary,
            mirror_of,
            mode: o.mode,
        });
    }
    let mut config = arrangement::NativeConfiguration { displays };
    config.canonicalize();
    Ok(config)
}

impl DisplayBackend for X11Display {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        let mut crtcs = std::collections::BTreeSet::new();
        Ok(self
            .outputs()?
            .into_iter()
            .filter(|o| crtcs.insert(o.crtc))
            .map(|o| o.info)
            .collect())
    }
    fn native_configuration(&self) -> Result<arrangement::NativeConfiguration, DisplayError> {
        configuration(self.outputs()?)
    }
    fn apply_native_origins(
        &self,
        expected: &arrangement::NativeConfiguration,
        origins: &[arrangement::NativeOrigin],
    ) -> Result<arrangement::NativeConfiguration, DisplayError> {
        native_arrangement::apply(self, expected, origins)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), DisplayError> {
        let (connection, screen) = connect()?;
        let root = connection.setup().roots[screen].root;
        connection
            .randr_select_input(
                root,
                randr::NotifyMask::SCREEN_CHANGE
                    | randr::NotifyMask::CRTC_CHANGE
                    | randr::NotifyMask::OUTPUT_CHANGE
                    | randr::NotifyMask::OUTPUT_PROPERTY,
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        connection.flush().map_err(error)?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let worker = thread::Builder::new()
            .name("oink-x11-displays".into())
            .spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let mut changed = false;
                    loop {
                        match connection.poll_for_event() {
                            Ok(Some(Event::RandrNotify(_))) | Ok(Some(Event::RandrScreenChangeNotify(_))) => {
                                changed = true
                            }
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(e) => {
                                tracing::warn!(%e, "X11 display subscription disconnected");
                                return;
                            }
                        }
                    }
                    if changed {
                        handler();
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            })
            .map_err(error)?;
        self.watches.lock().unwrap_or_else(|e| e.into_inner()).push(Watch {
            stop,
            thread: Some(worker),
        });
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_wayland_even_when_xwayland_sets_display() {
        assert!(!session_supported(Some("wayland"), true));
        assert!(!session_supported(None, true));
        assert!(session_supported(Some("x11"), true));
        assert!(session_supported(None, false));
    }
    #[test]
    fn edid_identity_is_deterministic_and_content_sensitive() {
        assert_eq!(edid_hash(b"monitor"), edid_hash(b"monitor"));
        assert_ne!(edid_hash(b"monitor-A"), edid_hash(b"monitor-B"));
    }
    #[test]
    fn rotation_reports_clockwise_quadrants() {
        assert_eq!(rotation_degrees(randr::Rotation::ROTATE90), 90);
        assert_eq!(rotation_degrees(randr::Rotation::ROTATE270), 270);
    }
}
