//! XDND v3-v5, COPY only. The incoming third-party drag remains alive while
//! held; Finished is sent only after its actual Drop and the remote outcome.
use super::{
    payload,
    x11::{self, X11, error},
};
use crate::{ClipData, ClipboardError, content, dnd::*};
use oink_foundation::{
    Point,
    input::{InjectEvent, MouseButton},
};
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{Event, shape::SK, xfixes::ConnectionExt as _, xproto::*},
    wrapper::ConnectionExt as _,
};

#[derive(Default)]
struct Controls {
    press: Option<SourcePress>,
    start: Option<SourceStart>,
    hold: Option<SourceHoldInput>,
    contents: HashMap<DragOperation, Arc<dyn content::Provider>>,
    ended: HashMap<DragOperation, bool>,
    began: HashMap<DragOperation, Instant>,
}
struct Shared {
    controls: Mutex<Controls>,
    ended: Condvar,
}
type Call = Box<dyn FnOnce(&mut Worker) + Send>;
struct Handle {
    tx: mpsc::Sender<Option<Call>>,
}
impl Handle {
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Worker) -> Result<T, ClipboardError> + Send + 'static,
    ) -> Result<T, ClipboardError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Some(Box::new(move |worker| {
                let _ = tx.send(f(worker));
            })))
            .map_err(error)?;
        rx.recv().map_err(error)?
    }
}
pub(crate) struct Backend {
    handle: Mutex<Option<Handle>>,
    shared: Arc<Shared>,
}
pub(crate) fn backend() -> Result<Box<dyn DndBackend>, ClipboardError> {
    x11::require_x11()?;
    Ok(Box::new(Backend {
        handle: Mutex::new(None),
        shared: Arc::new(Shared {
            controls: Mutex::new(Controls::default()),
            ended: Condvar::new(),
        }),
    }))
}
impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.tx.send(None);
        }
    }
}
impl Backend {
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Worker) -> Result<T, ClipboardError> + Send + 'static,
    ) -> Result<T, ClipboardError> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(ClipboardError::Unsupported)?
            .call(f)
    }
    fn finish(&self, operation: DragOperation, accept: bool, release: &dyn Fn() -> Result<(), String>) -> bool {
        if self
            .call(move |worker| {
                let source = worker
                    .incoming
                    .as_mut()
                    .filter(|s| s.operation == Some(operation))
                    .ok_or(ClipboardError::Busy)?;
                source.finishing = Some(accept);
                Ok(())
            })
            .is_err()
        {
            return false;
        }
        if release().is_err() {
            return false;
        }
        let controls = self.shared.controls.lock().unwrap();
        let (mut controls, _) = self
            .shared
            .ended
            .wait_timeout_while(controls, Duration::from_secs(2), |c| !c.ended.contains_key(&operation))
            .unwrap();
        controls
            .ended
            .remove(&operation)
            .is_some_and(|accepted| accepted == accept)
    }
}
impl DndBackend for Backend {
    fn start(&self, sink: DndSink, inject: Injector) -> Result<(), ClipboardError> {
        let mut slot = self.handle.lock().unwrap();
        if slot.is_some() {
            return Err(ClipboardError::Busy);
        }
        let (tx, rx) = mpsc::channel::<Option<Call>>();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("oink-x11-dnd".into())
            .spawn(move || match Worker::new(shared, sink, inject) {
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
                Ok(mut worker) => {
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    loop {
                        match rx.recv_timeout(Duration::from_millis(5)) {
                            Ok(Some(call)) => call(&mut worker),
                            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                let _ = worker.cancel_target(false);
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        if let Err(e) = worker.tick() {
                            tracing::debug!(reason = %e, "X11 drag peer unavailable");
                            let _ = worker.cancel_target(true);
                            if let Some(source) = worker.incoming.take() {
                                if source.held && !source.resumed {
                                    if let Some(operation) = source.operation {
                                        (worker.sink)(PlatformDndEvent::SourceCatcherLost { operation });
                                    }
                                }
                            }
                            let _ = worker.hide_catcher();
                        }
                    }
                }
            })
            .map_err(error)?;
        ready_rx.recv().map_err(error)??;
        *slot = Some(Handle { tx });
        Ok(())
    }
    fn set_source_press(&self, press: SourcePress) {
        self.shared.controls.lock().unwrap().press = Some(press);
    }
    fn set_source_start(&self, start: SourceStart) {
        self.shared.controls.lock().unwrap().start = Some(start);
    }
    fn set_source_hold_input(&self, input: SourceHoldInput) {
        self.shared.controls.lock().unwrap().hold = Some(input);
    }
    fn source_hold_installs_anchor(&self) -> bool {
        true
    }
    fn source_content(&self, operation: DragOperation) -> Option<Arc<dyn content::Provider>> {
        self.shared.controls.lock().unwrap().contents.get(&operation).cloned()
    }
    fn forget_content(&self, operation: DragOperation) {
        self.shared.controls.lock().unwrap().contents.remove(&operation);
    }
    fn probe(&self, at: Point) {
        let _ = self.call(move |w| w.probe(at));
    }
    fn unprobe(&self) {
        let _ = self.call(|w| {
            if w.incoming.as_ref().is_none_or(|s| !s.held) {
                w.hide_catcher()?;
            }
            Ok(())
        });
    }
    fn unprobe_source(&self, operation: DragOperation) {
        let _ = self.call(move |w| {
            if w.incoming.as_ref().is_some_and(|s| s.operation == Some(operation)) {
                w.hide_catcher()?;
                w.incoming = None;
            }
            Ok(())
        });
    }
    fn hold_source(&self, operation: DragOperation) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
        self.call(move |w| {
            let hold = w
                .shared
                .controls
                .lock()
                .unwrap()
                .hold
                .clone()
                .ok_or(ClipboardError::Unsupported)?;
            let source = w
                .incoming
                .as_mut()
                .filter(|s| s.operation == Some(operation))
                .ok_or(ClipboardError::Busy)?;
            let anchor = (hold.freeze)(operation).map_err(error)?;
            source.held = true;
            Ok(anchor)
        })
    }
    fn resume_source(&self, operation: DragOperation) -> bool {
        self.resume_source_at(operation, None)
    }
    fn resume_source_at(&self, operation: DragOperation, at: Option<Point>) -> bool {
        self.call(move |w| {
            let input = w
                .shared
                .controls
                .lock()
                .unwrap()
                .hold
                .clone()
                .ok_or(ClipboardError::Unsupported)?;
            let source = w
                .incoming
                .as_mut()
                .filter(|s| s.operation == Some(operation))
                .ok_or(ClipboardError::Busy)?;
            source.resumed = true;
            source.held = false;
            if let Some(at) = at {
                (input.return_to)(operation, at, &|_| {}).map_err(error)?;
            }
            w.hide_catcher()?;
            Ok(())
        })
        .is_ok()
    }
    fn prepare_source_cancel(
        &self,
        operation: DragOperation,
        at: Point,
        move_source: &dyn Fn(Point) -> Result<(), String>,
    ) -> bool {
        if self
            .call(move |w| {
                if !w.incoming.as_ref().is_some_and(|s| s.operation == Some(operation)) {
                    return Err(ClipboardError::Busy);
                }
                w.probe(at)
            })
            .is_err()
        {
            return false;
        }
        move_source(at).is_ok()
    }
    fn finish_source(&self, operation: DragOperation, accept: bool, release: &dyn Fn() -> Result<(), String>) -> bool {
        self.finish(operation, accept, release)
    }
    fn source_completion_is_async(&self) -> bool {
        true
    }
    fn release(&self) -> bool {
        let Ok(Some((operation, inject))) = self.call(|w| {
            Ok(w.incoming
                .as_ref()
                .and_then(|s| s.operation)
                .map(|op| (op, w.inject.clone())))
        }) else {
            return false;
        };
        self.finish(operation, true, &|| {
            inject(&InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            })
        })
    }
    fn release_physical(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        let Ok(Some(operation)) = self.call(|w| Ok(w.incoming.as_ref().and_then(|s| s.operation))) else {
            return false;
        };
        self.finish(operation, true, release)
    }
    fn begin(&self, spec: DragSpec) -> Result<(), ClipboardError> {
        if !spec.ops.contains(DragOps::COPY) {
            return Err(ClipboardError::Unsupported);
        }
        let publication = if let Some(native) = &spec.native {
            payload::Publication::native(native, Arc::new(|| Ok(())))?
        } else {
            payload::Publication::normalized(spec.items)
        };
        self.call(move |w| w.begin(spec.operation, spec.at, publication))
    }
    fn observe_native_begin(
        &self,
        operation: DragOperation,
        _: bool,
        callback: Box<dyn FnOnce(Option<Instant>) + Send>,
    ) {
        callback(self.shared.controls.lock().unwrap().began.get(&operation).copied());
    }
    fn target_start_timing(&self, operation: DragOperation, timing: Option<NativeBeginTiming>) {
        if let Some(timing) = timing {
            timing.complete(self.shared.controls.lock().unwrap().began.get(&operation).copied());
        }
    }
    fn cancel(&self) -> bool {
        self.call(|w| w.cancel_target(true)).is_ok()
    }
    fn cancel_physical(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        release().is_ok() && self.call(|w| w.cancel_target(false)).is_ok()
    }
}

struct Atoms {
    aware: Atom,
    proxy: Atom,
    enter: Atom,
    leave: Atom,
    position: Atom,
    status: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    types: Atom,
    actions: Atom,
    copy: Atom,
}
impl Atoms {
    fn new(x: &X11) -> Result<Self, ClipboardError> {
        Ok(Self {
            aware: x.atom("XdndAware")?,
            proxy: x.atom("XdndProxy")?,
            enter: x.atom("XdndEnter")?,
            leave: x.atom("XdndLeave")?,
            position: x.atom("XdndPosition")?,
            status: x.atom("XdndStatus")?,
            drop: x.atom("XdndDrop")?,
            finished: x.atom("XdndFinished")?,
            selection: x.atom("XdndSelection")?,
            types: x.atom("XdndTypeList")?,
            actions: x.atom("XdndActionList")?,
            copy: x.atom("XdndActionCopy")?,
        })
    }
}
struct Incoming {
    source: Window,
    version: u32,
    offered: Vec<Atom>,
    operation: Option<DragOperation>,
    time: u32,
    held: bool,
    resumed: bool,
    finishing: Option<bool>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Destination {
    logical: Window,
    recipient: Window,
    version: u32,
}
struct Outgoing {
    operation: DragOperation,
    target: Option<Destination>,
    accepted: bool,
    waiting: Option<Instant>,
    last: Option<(i16, i16)>,
    pressed: bool,
}
struct Worker {
    x: X11,
    catcher: Window,
    atoms: Atoms,
    shared: Arc<Shared>,
    sink: DndSink,
    inject: Injector,
    incoming: Option<Incoming>,
    outgoing: Option<Outgoing>,
    begin_pressed: bool,
    last_poll: Instant,
}
impl Worker {
    fn new(shared: Arc<Shared>, sink: DndSink, inject: Injector) -> Result<Self, ClipboardError> {
        let x = X11::new()?;
        x.conn
            .xfixes_query_version(5, 0)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let atoms = Atoms::new(&x)?;
        let catcher = x.conn.generate_id().map_err(error)?;
        x.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                catcher,
                x.root,
                0,
                0,
                3,
                3,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        for window in [x.window, catcher] {
            x.conn
                .change_property32(PropMode::REPLACE, window, atoms.aware, AtomEnum::ATOM, &[5])
                .map_err(error)?
                .check()
                .map_err(error)?;
            x.conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    x.atom("_NET_WM_WINDOW_OPACITY")?,
                    AtomEnum::CARDINAL,
                    &[0],
                )
                .map_err(error)?
                .check()
                .map_err(error)?;
            x.conn
                .change_window_attributes(window, &ChangeWindowAttributesAux::new().override_redirect(1))
                .map_err(error)?
                .check()
                .map_err(error)?;
        }
        x.conn.flush().map_err(error)?;
        Ok(Self {
            x,
            catcher,
            atoms,
            shared,
            sink,
            inject,
            incoming: None,
            outgoing: None,
            begin_pressed: false,
            last_poll: Instant::now(),
        })
    }
    fn send(&self, recipient: Window, logical: Window, kind: Atom, data: [u32; 5]) -> Result<(), ClipboardError> {
        let event = ClientMessageEvent::new(32, logical, kind, data);
        self.x
            .conn
            .send_event(false, recipient, EventMask::NO_EVENT, event)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x.conn.flush().map_err(error)
    }
    fn probe(&mut self, at: Point) -> Result<(), ClipboardError> {
        if self.outgoing.is_some() {
            return Ok(());
        }
        self.x
            .conn
            .configure_window(
                self.catcher,
                &ConfigureWindowAux::new()
                    .x(at.x.round() as i32 - 1)
                    .y(at.y.round() as i32 - 1)
                    .width(3)
                    .height(3)
                    .stack_mode(StackMode::ABOVE),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .map_window(self.catcher)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x.conn.flush().map_err(error)
    }
    fn hide_catcher(&self) -> Result<(), ClipboardError> {
        self.x
            .conn
            .unmap_window(self.catcher)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x.conn.flush().map_err(error)
    }
    fn incoming_status(&self, accepted: bool) -> Result<(), ClipboardError> {
        if let Some(source) = &self.incoming {
            self.send(
                source.source,
                source.source,
                self.atoms.status,
                [
                    self.catcher,
                    if accepted { 3 } else { 2 },
                    0,
                    0,
                    if accepted { self.atoms.copy } else { 0 },
                ],
            )?;
        }
        Ok(())
    }
    fn complete_source(&mut self, accepted: bool) -> Result<(), ClipboardError> {
        if let Some(source) = self.incoming.take() {
            self.send(
                source.source,
                source.source,
                self.atoms.finished,
                [
                    self.catcher,
                    u32::from(accepted),
                    if accepted { self.atoms.copy } else { 0 },
                    0,
                    0,
                ],
            )?;
            if let Some(operation) = source.operation {
                let mut controls = self.shared.controls.lock().unwrap();
                if source.finishing.is_some() {
                    controls.ended.insert(operation, accepted);
                }
                controls.contents.remove(&operation);
                drop(controls);
                self.shared.ended.notify_all();
                (self.sink)(PlatformDndEvent::SourceEnded { operation, accepted });
            }
        }
        self.hide_catcher()
    }
    fn begin(
        &mut self,
        operation: DragOperation,
        at: Point,
        publication: payload::Publication,
    ) -> Result<(), ClipboardError> {
        if self.outgoing.is_some() || self.incoming.as_ref().is_some_and(|s| s.held) {
            return Err(ClipboardError::Busy);
        }
        let result = self.begin_inner(operation, at, publication);
        if result.is_err() {
            let pressed = self.begin_pressed || self.outgoing.as_ref().is_some_and(|s| s.pressed);
            if pressed {
                let _ = (self.inject)(&InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                });
            }
            self.begin_pressed = false;
            self.outgoing = None;
            self.shared.controls.lock().unwrap().began.remove(&operation);
            let _ = self.x.conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = self.x.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            self.x.clear(self.atoms.selection);
            let _ = self.x.conn.unmap_window(self.x.window);
            let _ = self.x.conn.flush();
        }
        result
    }
    fn begin_inner(
        &mut self,
        operation: DragOperation,
        at: Point,
        publication: payload::Publication,
    ) -> Result<(), ClipboardError> {
        let (formats, lazy) = publication.formats(&self.x, crate::Sensitivity::Normal)?;
        let mut types: Vec<_> = formats.keys().chain(lazy.keys()).copied().collect();
        types.sort_unstable();
        types.dedup();
        self.x
            .conn
            .change_property32(
                PropMode::REPLACE,
                self.x.window,
                self.atoms.types,
                AtomEnum::ATOM,
                &types,
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .change_property32(
                PropMode::REPLACE,
                self.x.window,
                self.atoms.actions,
                AtomEnum::ATOM,
                &[self.atoms.copy],
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .publish_lazy(self.atoms.selection, formats, lazy, x11rb::CURRENT_TIME)?;
        self.hide_catcher()?;
        self.x
            .conn
            .xfixes_set_window_shape_region(self.x.window, SK::INPUT, 0, 0, x11rb::NONE)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .configure_window(
                self.x.window,
                &ConfigureWindowAux::new()
                    .x(at.x.round() as i32 - 1)
                    .y(at.y.round() as i32 - 1)
                    .width(3)
                    .height(3)
                    .stack_mode(StackMode::ABOVE),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .map_window(self.x.window)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x.conn.flush().map_err(error)?;
        let (press, start) = {
            let controls = self.shared.controls.lock().unwrap();
            (controls.press.clone(), controls.start.clone())
        };
        let inject = self.inject.clone();
        let mut submission = Ok(());
        let mut submit = || {
            submission = if let Some(press) = &press {
                press(operation, at)
            } else {
                inject(&InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                })
            };
        };
        let submitted = if let Some(start) = start {
            start(&mut submit).map(|_| ())
        } else {
            submit();
            Ok(())
        };
        if let Err(e) = submitted.and(submission) {
            self.x.clear(self.atoms.selection);
            let _ = self.x.conn.unmap_window(self.x.window);
            return Err(error(e));
        }
        self.begin_pressed = true;
        let grab = self
            .x
            .conn
            .grab_pointer(
                false,
                self.x.window,
                EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        if grab.status != GrabStatus::SUCCESS {
            self.x.clear(self.atoms.selection);
            let _ = self.x.conn.unmap_window(self.x.window);
            return Err(ClipboardError::Busy);
        }
        // Keep the grab window viewable, but remove it from pointer hit tests.
        // Otherwise our tiny source surface masks the actual drop target until
        // the user moves several pixels, and an immediate Drop always fails.
        let input_region = self.x.conn.generate_id().map_err(error)?;
        self.x
            .conn
            .xfixes_create_region(input_region, &[])
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .xfixes_set_window_shape_region(self.x.window, SK::INPUT, 0, 0, input_region)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .xfixes_destroy_region(input_region)
            .map_err(error)?
            .check()
            .map_err(error)?;
        let _ = self.x.conn.grab_keyboard(
            false,
            self.x.window,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
        self.outgoing = Some(Outgoing {
            operation,
            target: None,
            accepted: false,
            waiting: None,
            last: None,
            pressed: true,
        });
        self.begin_pressed = false;
        self.shared
            .controls
            .lock()
            .unwrap()
            .began
            .insert(operation, Instant::now());
        self.x.conn.flush().map_err(error)?;
        self.position(x11rb::CURRENT_TIME)
    }
    fn atoms_property(&self, window: Window, property: Atom) -> Vec<u32> {
        self.x
            .conn
            .get_property(false, window, property, AtomEnum::ATOM, 0, 4096)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }
    fn destination(&self) -> Result<(Option<Destination>, i16, i16, bool), ClipboardError> {
        let pointer = self
            .x
            .conn
            .query_pointer(self.x.root)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let mut chain = Vec::new();
        let mut child = pointer.child;
        for _ in 0..32 {
            if child == 0 || child == self.x.window || child == self.catcher {
                break;
            }
            chain.push(child);
            child = self
                .x
                .conn
                .query_pointer(child)
                .map_err(error)?
                .reply()
                .map_err(error)?
                .child;
        }
        let mut destination = None;
        for logical in chain.into_iter().rev() {
            let aware = self
                .atoms_property(logical, self.atoms.aware)
                .first()
                .copied()
                .unwrap_or(0);
            if aware < 3 {
                continue;
            }
            let mut recipient = logical;
            let proxy = self
                .x
                .conn
                .get_property(false, logical, self.atoms.proxy, AtomEnum::WINDOW, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().and_then(|mut v| v.next()));
            if let Some(proxy) = proxy {
                let confirmed = self
                    .x
                    .conn
                    .get_property(false, proxy, self.atoms.proxy, AtomEnum::WINDOW, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().and_then(|mut v| v.next()))
                    == Some(proxy);
                if confirmed
                    && self
                        .atoms_property(proxy, self.atoms.aware)
                        .first()
                        .copied()
                        .unwrap_or(0)
                        >= 3
                {
                    recipient = proxy;
                }
            }
            destination = Some(Destination {
                logical,
                recipient,
                version: aware.min(5),
            });
            break;
        }
        Ok((
            destination,
            pointer.root_x,
            pointer.root_y,
            pointer.mask.contains(KeyButMask::BUTTON1),
        ))
    }
    fn position(&mut self, time: u32) -> Result<(), ClipboardError> {
        if self.outgoing.as_ref().is_none_or(|s| s.waiting.is_some()) {
            return Ok(());
        }
        let (destination, x, y, down) = self.destination()?;
        let (previous, last) = self.outgoing.as_ref().map(|s| (s.target, s.last)).unwrap();
        if destination != previous {
            if let Some(previous) = previous {
                self.send(
                    previous.recipient,
                    previous.logical,
                    self.atoms.leave,
                    [self.x.window, 0, 0, 0, 0],
                )?;
            }
            if let Some(target) = destination {
                let types = self.atoms_property(self.x.window, self.atoms.types);
                self.send(
                    target.recipient,
                    target.logical,
                    self.atoms.enter,
                    [
                        self.x.window,
                        (target.version << 24) | u32::from(types.len() > 3),
                        types.first().copied().unwrap_or(0),
                        types.get(1).copied().unwrap_or(0),
                        types.get(2).copied().unwrap_or(0),
                    ],
                )?;
            }
            self.outgoing.as_mut().unwrap().target = destination;
            self.outgoing.as_mut().unwrap().accepted = false;
        }
        if destination != previous || last != Some((x, y)) {
            if let Some(target) = destination {
                self.send(
                    target.recipient,
                    target.logical,
                    self.atoms.position,
                    [
                        self.x.window,
                        0,
                        (u32::from(x as u16) << 16) | u32::from(y as u16),
                        time,
                        self.atoms.copy,
                    ],
                )?;
            }
            self.outgoing.as_mut().unwrap().last = Some((x, y));
        }
        if !down && self.outgoing.as_ref().is_some_and(|s| s.pressed) {
            self.drop_target(time)?;
        }
        Ok(())
    }
    fn drop_target(&mut self, time: u32) -> Result<(), ClipboardError> {
        let Some(source) = self.outgoing.as_mut() else {
            return Ok(());
        };
        if source.waiting.is_some() {
            return Ok(());
        }
        source.pressed = false;
        let target = source.target.filter(|_| source.accepted);
        self.x
            .conn
            .ungrab_pointer(time)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.x
            .conn
            .ungrab_keyboard(time)
            .map_err(error)?
            .check()
            .map_err(error)?;
        if let Some(target) = target {
            self.send(
                target.recipient,
                target.logical,
                self.atoms.drop,
                [self.x.window, 0, time, 0, 0],
            )?;
            self.outgoing.as_mut().unwrap().waiting = Some(Instant::now() + Duration::from_secs(5));
            Ok(())
        } else {
            self.cancel_target(false)
        }
    }
    fn end_target(&mut self, dropped: bool) -> Result<(), ClipboardError> {
        let Some(source) = self.outgoing.take() else {
            return Ok(());
        };
        self.x.conn.ungrab_pointer(x11rb::CURRENT_TIME).map_err(error)?;
        self.x.conn.ungrab_keyboard(x11rb::CURRENT_TIME).map_err(error)?;
        self.x.clear(self.atoms.selection);
        self.x.conn.unmap_window(self.x.window).map_err(error)?;
        self.x.conn.flush().map_err(error)?;
        self.shared.controls.lock().unwrap().began.remove(&source.operation);
        (self.sink)(PlatformDndEvent::Ended {
            operation: source.operation,
            dropped,
        });
        Ok(())
    }
    fn cancel_target(&mut self, release: bool) -> Result<(), ClipboardError> {
        if let Some(source) = &self.outgoing {
            if let Some(target) = source.target {
                self.send(
                    target.recipient,
                    target.logical,
                    self.atoms.leave,
                    [self.x.window, 0, 0, 0, 0],
                )?;
            }
            if release && source.pressed {
                (self.inject)(&InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                })
                .map_err(error)?;
            }
        }
        self.end_target(false)
    }
    fn client(&mut self, event: ClientMessageEvent) -> Result<(), ClipboardError> {
        if event.format != 32 {
            return Ok(());
        }
        let data = event.data.as_data32();
        if event.window == self.catcher {
            if event.type_ == self.atoms.enter {
                if self.incoming.as_ref().is_some_and(|s| s.held) {
                    return Ok(());
                }
                if let Some(previous) = self.incoming.take().and_then(|s| s.operation) {
                    self.shared.controls.lock().unwrap().contents.remove(&previous);
                }
                let version = data[1] >> 24;
                if version < 3 {
                    return Ok(());
                }
                let offered = if data[1] & 1 != 0 {
                    self.atoms_property(data[0], self.atoms.types)
                } else {
                    data[2..5].iter().copied().filter(|a| *a != 0).collect()
                };
                self.incoming = Some(Incoming {
                    source: data[0],
                    version,
                    offered,
                    operation: None,
                    time: 0,
                    held: false,
                    resumed: false,
                    finishing: None,
                });
            } else if event.type_ == self.atoms.position && self.incoming.as_ref().is_some_and(|s| s.source == data[0])
            {
                let offered = self.incoming.as_ref().unwrap().offered.clone();
                let copy = data[4] == self.atoms.copy
                    || self
                        .atoms_property(data[0], self.atoms.actions)
                        .contains(&self.atoms.copy);
                self.incoming.as_mut().unwrap().time = data[3];
                if !copy {
                    return self.incoming_status(false);
                }
                if self.incoming.as_ref().unwrap().operation.is_none() {
                    match payload::read_offered(&mut self.x, self.atoms.selection, data[3], &offered) {
                        Ok(items) if !items.is_empty() => {
                            let operation = DragOperation::new();
                            let provider = payload::owned(&items, operation.sequence)?;
                            let native = content::items(&provider.inventory());
                            self.shared
                                .controls
                                .lock()
                                .unwrap()
                                .contents
                                .insert(operation, provider);
                            self.incoming.as_mut().unwrap().operation = Some(operation);
                            self.incoming_status(true)?;
                            (self.sink)(PlatformDndEvent::Detected {
                                operation,
                                items: native,
                                ops: DragOps::COPY,
                            });
                        }
                        _ => self.incoming_status(false)?,
                    }
                } else {
                    self.incoming_status(true)?;
                }
            } else if event.type_ == self.atoms.drop && self.incoming.as_ref().is_some_and(|s| s.source == data[0]) {
                let accepted = self
                    .incoming
                    .as_ref()
                    .is_some_and(|s| s.finishing == Some(true) && s.operation.is_some());
                self.complete_source(accepted)?;
            } else if event.type_ == self.atoms.leave && self.incoming.as_ref().is_some_and(|s| s.source == data[0]) {
                let source = self.incoming.take().unwrap();
                if let Some(operation) = source.operation {
                    self.shared.controls.lock().unwrap().contents.remove(&operation);
                    if source.finishing == Some(false) {
                        self.shared.controls.lock().unwrap().ended.insert(operation, false);
                        self.shared.ended.notify_all();
                        (self.sink)(PlatformDndEvent::SourceEnded {
                            operation,
                            accepted: false,
                        });
                    } else if source.held && !source.resumed {
                        (self.sink)(PlatformDndEvent::SourceCatcherLost { operation });
                    }
                }
            }
        } else if event.window == self.x.window {
            if event.type_ == self.atoms.status {
                if let Some(source) = self
                    .outgoing
                    .as_mut()
                    .filter(|s| s.target.is_some_and(|t| t.logical == data[0] || t.recipient == data[0]))
                {
                    source.accepted = data[1] & 1 != 0 && data[4] == self.atoms.copy;
                }
            } else if event.type_ == self.atoms.finished
                && self.outgoing.as_ref().is_some_and(|s| {
                    s.waiting.is_some() && s.target.is_some_and(|t| t.logical == data[0] || t.recipient == data[0])
                })
            {
                let target_version = self.outgoing.as_ref().and_then(|s| s.target).map_or(5, |t| t.version);
                self.end_target(target_version < 5 || (data[1] & 1 != 0 && data[2] == self.atoms.copy))?;
            }
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<(), ClipboardError> {
        while let Some(event) = self.x.next()? {
            match event {
                Event::ClientMessage(message) => self.client(message)?,
                Event::MotionNotify(event) if self.outgoing.is_some() => self.position(event.time)?,
                Event::ButtonRelease(event) if event.detail == 1 && self.outgoing.is_some() => {
                    self.drop_target(event.time)?
                }
                Event::KeyPress(event) if self.outgoing.is_some() => {
                    let is_escape = self
                        .x
                        .conn
                        .get_keyboard_mapping(event.detail, 1)
                        .map_err(error)?
                        .reply()
                        .map_err(error)?
                        .keysyms
                        .contains(&0xff1b);
                    if is_escape {
                        self.cancel_target(true)?;
                    }
                }
                _ => {
                    if let Err(e) = self.x.service(&event) {
                        tracing::debug!(reason = %e, "X11 drag selection requester retired");
                    }
                }
            }
        }
        if self
            .outgoing
            .as_ref()
            .is_some_and(|s| s.waiting.is_some_and(|deadline| deadline <= Instant::now()))
        {
            self.end_target(false)?;
        }
        if self.outgoing.is_some() && self.last_poll.elapsed() >= Duration::from_millis(16) {
            self.last_poll = Instant::now();
            self.position(x11rb::CURRENT_TIME)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xtest::ConnectionExt as _;
    fn send(x: &X11, destination: Window, kind: Atom, data: [u32; 5]) {
        x.conn
            .send_event(
                false,
                destination,
                EventMask::NO_EVENT,
                ClientMessageEvent::new(32, destination, kind, data),
            )
            .unwrap()
            .check()
            .unwrap();
        x.conn.flush().unwrap();
    }
    fn wait(mut ready: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(4);
        while !ready() {
            assert!(Instant::now() < end, "XDND peer did not converge");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn device() -> (
        Backend,
        mpsc::Receiver<PlatformDndEvent>,
        Arc<x11rb::rust_connection::RustConnection>,
    ) {
        super::super::test_server();
        let backend = Backend {
            handle: Mutex::new(None),
            shared: Arc::new(Shared {
                controls: Mutex::new(Controls::default()),
                ended: Condvar::new(),
            }),
        };
        let (conn, _) = x11rb::connect(None).unwrap();
        let conn = Arc::new(conn);
        let input = conn.clone();
        let (tx, rx) = mpsc::channel();
        backend
            .start(
                Arc::new(move |e| {
                    let _ = tx.send(e);
                }),
                Arc::new(move |event| {
                    if let InjectEvent::Button {
                        button: MouseButton::Left,
                        pressed,
                    } = event
                    {
                        input
                            .xtest_fake_input(
                                if *pressed {
                                    BUTTON_PRESS_EVENT
                                } else {
                                    BUTTON_RELEASE_EVENT
                                },
                                1,
                                0,
                                0,
                                0,
                                0,
                                0,
                            )
                            .map_err(|e| e.to_string())?
                            .check()
                            .map_err(|e| e.to_string())?;
                        input.flush().map_err(|e| e.to_string())?;
                    }
                    Ok(())
                }),
            )
            .unwrap();
        let press = conn.clone();
        backend.set_source_press(Arc::new(move |_, at| {
            press
                .xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    0,
                    0,
                    press.setup().roots[0].root,
                    at.x as i16,
                    at.y as i16,
                    0,
                )
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|e| e.to_string())?;
            press
                .xtest_fake_input(BUTTON_PRESS_EVENT, 1, 0, 0, 0, 0, 0)
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|e| e.to_string())?;
            press.flush().map_err(|e| e.to_string())?;
            Ok(())
        }));
        (backend, rx, conn)
    }
    #[test]
    #[ignore = "requires an isolated X11 server with XTEST (Xvfb)"]
    fn incoming_xdnd_keeps_original_selection_until_real_drop() {
        let (backend, events, _) = device();
        backend.probe(Point { x: 50., y: 50. });
        let catcher = backend.call(|w| Ok(w.catcher)).unwrap();
        let mut source = X11::new().unwrap();
        let atoms = Atoms::new(&source).unwrap();
        let text = source.atom("UTF8_STRING").unwrap();
        let formats = payload::encode(
            &source,
            &[ClipData::Text("original drag".into())],
            crate::Sensitivity::Normal,
        )
        .unwrap();
        source.publish(atoms.selection, formats, 0).unwrap();
        send(&source, catcher, atoms.enter, [source.window, 5 << 24, text, 0, 0]);
        send(
            &source,
            catcher,
            atoms.position,
            [source.window, 0, (50 << 16) | 50, 0, atoms.copy],
        );
        let (drop_tx, drop_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let source_worker = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(8);
            while Instant::now() < end {
                if drop_rx.try_recv().is_ok() {
                    send(&source, catcher, atoms.drop, [source.window, 0, 0, 0, 0]);
                }
                while let Some(event) = source.next().unwrap() {
                    if let Event::ClientMessage(event) = &event {
                        if event.type_ == atoms.finished {
                            finished_tx.send(event.data.as_data32()[1] & 1 != 0).unwrap();
                            return;
                        }
                    }
                    source.service(&event).unwrap();
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            panic!("original XDND source never received Finished");
        });
        let detected = events.recv_timeout(Duration::from_secs(4)).unwrap();
        let PlatformDndEvent::Detected { operation, .. } = detected else {
            panic!("missing real drag detection")
        };
        assert!(
            finished_rx.try_recv().is_err(),
            "hover incorrectly completed original drag"
        );
        let provider = backend.source_content(operation).unwrap();
        let id = provider.inventory().items[0].representations[0].id;
        assert_eq!(
            content::read_all(&mut *provider.open(id).unwrap()).unwrap(),
            b"original drag"
        );
        assert!(backend.finish_source(operation, true, &|| drop_tx.send(()).map_err(|e| e.to_string())));
        assert!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(2)).unwrap(), PlatformDndEvent::SourceEnded { operation: ended, accepted: true } if ended == operation)
        );
        source_worker.join().unwrap();
    }
    #[test]
    #[ignore = "requires an isolated X11 server with XTEST (Xvfb)"]
    fn outgoing_xdnd_waits_for_real_target_consumption_and_finished() {
        for move_after_begin in [false, true] {
            let (backend, events, input) = device();
            let mut target = X11::new().unwrap();
            let atoms = Atoms::new(&target).unwrap();
            let origin = if move_after_begin { 200 } else { 0 };
            target
                .conn
                .configure_window(
                    target.window,
                    &ConfigureWindowAux::new()
                        .x(origin)
                        .y(origin)
                        .width(400)
                        .height(400)
                        .stack_mode(StackMode::ABOVE),
                )
                .unwrap()
                .check()
                .unwrap();
            target
                .conn
                .change_property32(PropMode::REPLACE, target.window, atoms.aware, AtomEnum::ATOM, &[5])
                .unwrap()
                .check()
                .unwrap();
            target.conn.map_window(target.window).unwrap().check().unwrap();
            target.conn.flush().unwrap();
            let target_worker = std::thread::spawn(move || {
                let end = Instant::now() + Duration::from_secs(8);
                let mut source = 0;
                while Instant::now() < end {
                    while let Some(event) = target.next().unwrap() {
                        if let Event::ClientMessage(event) = &event {
                            let data = event.data.as_data32();
                            if event.type_ == atoms.enter {
                                source = data[0];
                            } else if event.type_ == atoms.position {
                                send(&target, source, atoms.status, [target.window, 3, 0, 0, atoms.copy]);
                            } else if event.type_ == atoms.drop {
                                let utf8 = target.atom("UTF8_STRING").unwrap();
                                assert_eq!(target.load(atoms.selection, utf8, data[2]).unwrap(), b"remote drag");
                                send(&target, source, atoms.finished, [target.window, 1, atoms.copy, 0, 0]);
                                return;
                            }
                        }
                        target.service(&event).unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                panic!("target never received an XDND Drop");
            });
            let operation = DragOperation::new();
            backend
                .begin(DragSpec {
                    operation,
                    at: Point { x: 50., y: 50. },
                    items: vec![ClipData::Text("remote drag".into())],
                    files: None,
                    native: None,
                    ops: DragOps::COPY,
                })
                .unwrap();
            // Exercise both initial hit testing without any post-Begin motion and
            // the original motion/target-discovery path.
            if move_after_begin {
                input
                    .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, input.setup().roots[0].root, 250, 250, 0)
                    .unwrap()
                    .check()
                    .unwrap();
                input.flush().unwrap();
            }
            wait(|| {
                backend
                    .call(|w| Ok(w.outgoing.as_ref().is_some_and(|s| s.accepted)))
                    .unwrap()
            });
            assert!(
                events.try_recv().is_err(),
                "hover incorrectly reported a completed Drop"
            );
            input
                .xtest_fake_input(BUTTON_RELEASE_EVENT, 1, 0, 0, 0, 0, 0)
                .unwrap()
                .check()
                .unwrap();
            input.flush().unwrap();
            assert!(
                matches!(events.recv_timeout(Duration::from_secs(4)).unwrap(), PlatformDndEvent::Ended { operation: ended, dropped: true } if ended == operation)
            );
            target_worker.join().unwrap();
        }
    }
}
