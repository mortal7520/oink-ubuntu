//! ICCCM selections. One ordinary worker owns each connection and keeps serving
//! selections while callers wait. No X11 work runs on the input callback.
use crate::ClipboardError;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

pub(super) const LIMIT: usize = crate::model::REPRESENTATION_BYTES as usize;
const CHUNK: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
pub(super) type Formats = HashMap<Atom, Arc<Vec<u8>>>;
pub(super) type Loader = Arc<dyn Fn() -> Result<Vec<u8>, ClipboardError> + Send + Sync>;
pub(super) type LazyFormats = HashMap<Atom, Loader>;
pub(super) fn error(e: impl std::fmt::Display) -> ClipboardError {
    ClipboardError::Platform(e.to_string())
}

/// Do not silently connect to XWayland: it cannot observe the native Wayland
/// clipboard or perform the global XDND handoff this adapter promises.
pub(super) fn require_x11() -> Result<(), ClipboardError> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
        || (std::env::var_os("WAYLAND_DISPLAY").is_some()
            && !std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("x11")))
    {
        return Err(ClipboardError::Platform(
            "native Wayland clipboard and drag-and-drop are unsupported; select an X11 session".into(),
        ));
    }
    if std::env::var_os("DISPLAY").is_none() {
        return Err(ClipboardError::Platform("X11 DISPLAY is not set".into()));
    }
    Ok(())
}

struct Sending {
    bytes: Arc<Vec<u8>>,
    offset: usize,
    target: Atom,
    deadline: Instant,
}
struct Loaded {
    job: u64,
    result: Result<Vec<u8>, ClipboardError>,
}
struct Pending {
    request: SelectionRequestEvent,
    property: Atom,
    publication: u64,
    deadline: Instant,
}
pub(super) struct X11 {
    pub conn: RustConnection,
    pub window: Window,
    pub root: Window,
    pub targets: Atom,
    pub timestamp: Atom,
    pub incr: Atom,
    pub property: Atom,
    pub owned: HashMap<Atom, Formats>,
    pub timestamps: HashMap<Atom, Timestamp>,
    lazy: HashMap<Atom, LazyFormats>,
    publication: u64,
    loaded_tx: mpsc::Sender<Loaded>,
    loaded_rx: mpsc::Receiver<Loaded>,
    loading: usize,
    next_job: u64,
    pending: HashMap<u64, Pending>,
    outgoing: HashMap<(Window, Atom), Sending>,
    deferred: VecDeque<Event>,
}
impl X11 {
    pub fn new() -> Result<Self, ClipboardError> {
        require_x11()?;
        let (conn, screen) = x11rb::connect(None).map_err(error)?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().map_err(error)?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(error)?
        .check()
        .map_err(error)?;
        let atom = |name: &[u8]| -> Result<Atom, ClipboardError> {
            Ok(conn
                .intern_atom(false, name)
                .map_err(error)?
                .reply()
                .map_err(error)?
                .atom)
        };
        let targets = atom(b"TARGETS")?;
        let timestamp = atom(b"TIMESTAMP")?;
        let incr = atom(b"INCR")?;
        let property = atom(b"_OINK_SELECTION")?;
        conn.flush().map_err(error)?;
        let (loaded_tx, loaded_rx) = mpsc::channel();
        Ok(Self {
            conn,
            window,
            root,
            targets,
            timestamp,
            incr,
            property,
            owned: HashMap::new(),
            timestamps: HashMap::new(),
            lazy: HashMap::new(),
            publication: 0,
            loaded_tx,
            loaded_rx,
            loading: 0,
            next_job: 0,
            pending: HashMap::new(),
            outgoing: HashMap::new(),
            deferred: VecDeque::new(),
        })
    }
    pub fn atom(&self, name: &str) -> Result<Atom, ClipboardError> {
        Ok(self
            .conn
            .intern_atom(false, name.as_bytes())
            .map_err(error)?
            .reply()
            .map_err(error)?
            .atom)
    }
    pub fn owner(&self, selection: Atom) -> Result<Window, ClipboardError> {
        Ok(self
            .conn
            .get_selection_owner(selection)
            .map_err(error)?
            .reply()
            .map_err(error)?
            .owner)
    }
    pub fn publish(&mut self, selection: Atom, formats: Formats, time: Timestamp) -> Result<(), ClipboardError> {
        self.conn
            .set_selection_owner(self.window, selection, time)
            .map_err(error)?
            .check()
            .map_err(error)?;
        if self.owner(selection)? != self.window {
            return Err(ClipboardError::Busy);
        }
        self.owned.insert(selection, formats);
        self.lazy.remove(&selection);
        self.publication = self.publication.wrapping_add(1);
        self.timestamps.insert(selection, time);
        self.conn.flush().map_err(error)
    }
    pub fn publish_lazy(
        &mut self,
        selection: Atom,
        formats: Formats,
        lazy: LazyFormats,
        time: Timestamp,
    ) -> Result<(), ClipboardError> {
        self.publish(selection, formats, time)?;
        self.lazy.insert(selection, lazy);
        Ok(())
    }
    pub fn clear(&mut self, selection: Atom) {
        if self.owner(selection).ok() == Some(self.window) {
            let _ = self
                .conn
                .set_selection_owner(x11rb::NONE, selection, x11rb::CURRENT_TIME);
        }
        self.owned.remove(&selection);
        self.lazy.remove(&selection);
        self.publication = self.publication.wrapping_add(1);
        let _ = self.conn.flush();
    }
    pub fn next(&mut self) -> Result<Option<Event>, ClipboardError> {
        self.completed_loads();
        self.outgoing.retain(|_, v| v.deadline > Instant::now());
        if let Some(event) = self.deferred.pop_front() {
            return Ok(Some(event));
        }
        self.conn.poll_for_event().map_err(error)
    }
    fn notify(&self, req: &SelectionRequestEvent, property: Atom) -> Result<(), ClipboardError> {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        self.conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, event)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn.flush().map_err(error)
    }
    fn send_bytes(
        &mut self,
        req: &SelectionRequestEvent,
        property: Atom,
        bytes: Arc<Vec<u8>>,
    ) -> Result<(), ClipboardError> {
        if bytes.len() > LIMIT {
            return self.notify(req, x11rb::NONE);
        }
        if bytes.len() > CHUNK {
            if req.requestor != self.window {
                self.conn
                    .change_window_attributes(
                        req.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    )
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
            }
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    property,
                    self.incr,
                    &[bytes.len() as u32],
                )
                .map_err(error)?
                .check()
                .map_err(error)?;
            self.outgoing.insert(
                (req.requestor, property),
                Sending {
                    bytes,
                    offset: 0,
                    target: req.target,
                    deadline: Instant::now() + TIMEOUT,
                },
            );
        } else {
            self.conn
                .change_property8(PropMode::REPLACE, req.requestor, property, req.target, &bytes)
                .map_err(error)?
                .check()
                .map_err(error)?;
        }
        self.notify(req, property)
    }
    fn completed_loads(&mut self) {
        while let Ok(loaded) = self.loaded_rx.try_recv() {
            self.loading = self.loading.saturating_sub(1);
            let Some(pending) = self.pending.remove(&loaded.job) else {
                continue;
            };
            let result = if pending.publication == self.publication
                && self.owned.contains_key(&pending.request.selection)
                && Instant::now() < pending.deadline
            {
                match loaded.result {
                    Ok(bytes) => self.send_bytes(&pending.request, pending.property, Arc::new(bytes)),
                    Err(_) => self.notify(&pending.request, x11rb::NONE),
                }
            } else {
                self.notify(&pending.request, x11rb::NONE)
            };
            if let Err(e) = result {
                tracing::debug!(reason = %e, "X11 deferred request retired");
            }
        }
        // A network reader may itself be blocked. Bound the native protocol
        // independently: tell the toolkit failure at five seconds, discard a
        // late result, and keep the number of outstanding workers bounded.
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| Instant::now() >= p.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = self.notify(&pending.request, x11rb::NONE);
            }
        }
    }
    /// True means the event belonged to the common selection transport.
    pub fn service(&mut self, event: &Event) -> Result<bool, ClipboardError> {
        match event {
            Event::SelectionRequest(req) => {
                let property = if req.property == x11rb::NONE {
                    req.target
                } else {
                    req.property
                };
                let mut accepted = false;
                if let Some(formats) = self.owned.get(&req.selection) {
                    if req.target == self.targets {
                        let mut targets: Vec<_> = formats.keys().copied().collect();
                        if let Some(lazy) = self.lazy.get(&req.selection) {
                            targets.extend(lazy.keys().copied());
                        }
                        targets.extend([self.targets, self.timestamp]);
                        targets.sort_unstable();
                        targets.dedup();
                        self.conn
                            .change_property32(PropMode::REPLACE, req.requestor, property, AtomEnum::ATOM, &targets)
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                        accepted = true;
                    } else if req.target == self.timestamp {
                        self.conn
                            .change_property32(
                                PropMode::REPLACE,
                                req.requestor,
                                property,
                                AtomEnum::INTEGER,
                                &[self.timestamps.get(&req.selection).copied().unwrap_or(0)],
                            )
                            .map_err(error)?
                            .check()
                            .map_err(error)?;
                        accepted = true;
                    } else if let Some(bytes) = formats.get(&req.target) {
                        let bytes = bytes.clone();
                        return self.send_bytes(req, property, bytes).map(|()| true);
                    } else if let Some(loader) = self.lazy.get(&req.selection).and_then(|m| m.get(&req.target)).cloned()
                    {
                        if self.loading < 16 {
                            self.loading += 1;
                            self.next_job = self.next_job.wrapping_add(1);
                            let job = self.next_job;
                            self.pending.insert(
                                job,
                                Pending {
                                    request: *req,
                                    property,
                                    publication: self.publication,
                                    deadline: Instant::now() + TIMEOUT,
                                },
                            );
                            let tx = self.loaded_tx.clone();
                            std::thread::spawn(move || {
                                let result = loader();
                                let _ = tx.send(Loaded { job, result });
                            });
                            return Ok(true);
                        }
                    }
                }
                self.notify(req, if accepted { property } else { x11rb::NONE })?;
                Ok(true)
            }
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                let key = (event.window, event.atom);
                if let Some(send) = self.outgoing.get_mut(&key) {
                    let end = (send.offset + CHUNK).min(send.bytes.len());
                    self.conn
                        .change_property8(
                            PropMode::REPLACE,
                            event.window,
                            event.atom,
                            send.target,
                            &send.bytes[send.offset..end],
                        )
                        .map_err(error)?
                        .check()
                        .map_err(error)?;
                    let finished = send.offset == end;
                    send.offset = end;
                    send.deadline = Instant::now() + TIMEOUT;
                    if finished {
                        self.outgoing.remove(&key);
                    }
                    self.conn.flush().map_err(error)?;
                    return Ok(true);
                }
                Ok(false)
            }
            Event::SelectionClear(event) => {
                self.owned.remove(&event.selection);
                self.lazy.remove(&event.selection);
                Ok(true)
            }
            Event::DestroyNotify(event) => {
                self.outgoing.retain(|(w, _), _| *w != event.window);
                Ok(false)
            }
            _ => Ok(false),
        }
    }
    fn wait(&mut self, deadline: Instant, predicate: impl Fn(&Event) -> bool) -> Result<Event, ClipboardError> {
        loop {
            self.completed_loads();
            if Instant::now() >= deadline {
                return Err(ClipboardError::Platform("X11 selection transfer timed out".into()));
            }
            if let Some(event) = self.conn.poll_for_event().map_err(error)? {
                if predicate(&event) {
                    return Ok(event);
                }
                if !self.service(&event)? {
                    self.deferred.push_back(event);
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    pub fn load(&mut self, selection: Atom, target: Atom, time: Timestamp) -> Result<Vec<u8>, ClipboardError> {
        if let Some(bytes) = self.owned.get(&selection).and_then(|m| m.get(&target)) {
            return Ok(bytes.as_ref().clone());
        }
        let owner = self.owner(selection)?;
        if owner == x11rb::NONE {
            return Ok(Vec::new());
        }
        self.conn
            .delete_property(self.window, self.property)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn
            .convert_selection(self.window, selection, target, self.property, time)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.conn.flush().map_err(error)?;
        let window = self.window;
        let deadline = Instant::now() + TIMEOUT;
        let event = self.wait(deadline, |event| matches!(event, Event::SelectionNotify(e) if e.requestor == window && e.selection == selection && e.target == target))?;
        if matches!(event, Event::SelectionNotify(e) if e.property == x11rb::NONE) {
            return Err(ClipboardError::Unsupported);
        }
        let reply = self
            .conn
            .get_property(
                false,
                self.window,
                self.property,
                AtomEnum::ANY,
                0,
                (LIMIT / 4 + 1) as u32,
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let bytes = if reply.type_ != self.incr {
            self.conn.delete_property(self.window, self.property).map_err(error)?;
            if reply.bytes_after != 0 || reply.value.len() > LIMIT {
                return Err(ClipboardError::ContentUnavailable(
                    crate::model::Unavailable::LimitExceeded,
                ));
            }
            reply.value
        } else {
            if reply
                .value32()
                .and_then(|mut v| v.next())
                .is_some_and(|n| n as usize > LIMIT)
            {
                return Err(ClipboardError::ContentUnavailable(
                    crate::model::Unavailable::LimitExceeded,
                ));
            }
            self.conn
                .delete_property(self.window, self.property)
                .map_err(error)?
                .check()
                .map_err(error)?;
            self.conn.flush().map_err(error)?;
            let mut bytes = Vec::new();
            loop {
                let property = self.property;
                self.wait(deadline, |event| matches!(event, Event::PropertyNotify(e) if e.window == window && e.atom == property && e.state == Property::NEW_VALUE))?;
                let chunk = self
                    .conn
                    .get_property(true, self.window, property, AtomEnum::ANY, 0, (LIMIT / 4 + 1) as u32)
                    .map_err(error)?
                    .reply()
                    .map_err(error)?;
                if chunk.bytes_after != 0 || bytes.len().saturating_add(chunk.value.len()) > LIMIT {
                    return Err(ClipboardError::ContentUnavailable(
                        crate::model::Unavailable::LimitExceeded,
                    ));
                }
                if chunk.value.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk.value);
                self.conn.flush().map_err(error)?;
                if self.owner(selection)? != owner {
                    return Err(ClipboardError::Busy);
                }
            }
            bytes
        };
        self.conn.flush().map_err(error)?;
        if self.owner(selection)? != owner {
            return Err(ClipboardError::Busy);
        }
        Ok(bytes)
    }
    pub fn offered(&mut self, selection: Atom, time: Timestamp) -> Result<Vec<Atom>, ClipboardError> {
        if let Some(formats) = self.owned.get(&selection) {
            let mut targets: Vec<_> = formats.keys().copied().collect();
            if let Some(lazy) = self.lazy.get(&selection) {
                targets.extend(lazy.keys().copied());
            }
            return Ok(targets);
        }
        let bytes = self.load(selection, self.targets, time)?;
        if bytes.len() % 4 != 0 || bytes.len() > 65_536 {
            return Err(ClipboardError::Unsupported);
        }
        Ok(bytes
            .chunks_exact(4)
            .map(|v| u32::from_ne_bytes(v.try_into().unwrap()))
            .collect())
    }
}
