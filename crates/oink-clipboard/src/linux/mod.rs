//! X11 clipboard: CLIPBOARD ownership, XFixes observation and ICCCM INCR.
//! Native file lists refer to real, identity-checked paths; remote files are
//! materialized by the existing bounded transfer policy before publication.
pub(crate) mod dnd;
mod payload;
#[cfg(test)]
mod tests;
mod x11;
#[cfg(test)]
pub(super) fn test_server() {
    // Xvfb resets when its last client disconnects. Keep one display connection
    // for the test process, as a real desktop's window manager would, so a new
    // case cannot race that reset before any Oink backend code has run.
    static DISPLAY: std::sync::OnceLock<Arc<x11rb::rust_connection::RustConnection>> = std::sync::OnceLock::new();
    DISPLAY.get_or_init(|| Arc::new(x11rb::connect(None).expect("isolated X11 display").0));
}
use crate::content::{self, namespace::Namespace};
use crate::model::*;
use crate::{
    CapturedClipboard, ChangeHandler, ClipData, ClipSnapshot, ClipboardBackend, ClipboardError, NativeSpec, Sensitivity,
};
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
use x11::{Formats, X11, error};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::Atom,
    },
};

pub(super) struct Shared {
    commands: mpsc::Sender<Command>,
    count: AtomicU64,
    paused: AtomicBool,
    handlers: Mutex<Vec<ChangeHandler>>,
    session: [u8; 32],
}
enum Command {
    Call(Box<dyn FnOnce(&mut Worker) + Send>),
    Stop,
}
struct Worker {
    x: X11,
    clipboard: Atom,
    shared: Arc<Shared>,
    last_owner: u32,
    last_time: u32,
}
#[derive(Clone)]
pub(super) struct Handle(Arc<Shared>);
impl Handle {
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Worker) -> Result<T, ClipboardError> + Send + 'static,
    ) -> Result<T, ClipboardError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0
            .commands
            .send(Command::Call(Box::new(move |worker| {
                let _ = tx.send(f(worker));
            })))
            .map_err(error)?;
        rx.recv().map_err(error)?
    }
    pub fn load(&self, expected: u64, atom: Atom) -> io::Result<Vec<u8>> {
        self.call(move |worker| {
            if worker.shared.count.load(Ordering::Acquire) != expected {
                return Err(ClipboardError::Busy);
            }
            let bytes = worker.x.load(worker.clipboard, atom, x11rb::CURRENT_TIME)?;
            worker.drain()?;
            if worker.shared.count.load(Ordering::Acquire) != expected {
                return Err(ClipboardError::Busy);
            }
            Ok(bytes)
        })
        .map_err(io_error)
    }
}
pub(super) fn io_error(e: ClipboardError) -> io::Error {
    match e {
        ClipboardError::Busy => content::unavailable(Unavailable::Superseded),
        ClipboardError::Unsupported => content::unavailable(Unavailable::Unsupported),
        ClipboardError::ContentUnavailable(reason) => content::unavailable(reason),
        e => io::Error::other(e),
    }
}
struct Backend {
    handle: Handle,
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.handle.0.commands.send(Command::Stop);
    }
}
pub(super) fn backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    x11::require_x11()?;
    let (commands, receiver) = mpsc::channel();
    let (started, ready) = mpsc::sync_channel(1);
    let mut session = [0; 32];
    getrandom::fill(&mut session).map_err(error)?;
    let shared = Arc::new(Shared {
        commands,
        count: AtomicU64::new(1),
        paused: AtomicBool::new(false),
        handlers: Mutex::new(Vec::new()),
        session,
    });
    let owner = shared.clone();
    std::thread::Builder::new()
        .name("oink-x11-clipboard".into())
        .spawn(move || {
            let setup = (|| {
                let x = X11::new()?;
                x.conn
                    .xfixes_query_version(5, 0)
                    .map_err(error)?
                    .reply()
                    .map_err(error)?;
                let clipboard = x.atom("CLIPBOARD")?;
                x.conn
                    .xfixes_select_selection_input(
                        x.window,
                        clipboard,
                        SelectionEventMask::SET_SELECTION_OWNER
                            | SelectionEventMask::SELECTION_WINDOW_DESTROY
                            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                    )
                    .map_err(error)?
                    .check()
                    .map_err(error)?;
                x.conn.flush().map_err(error)?;
                let last_owner = x.owner(clipboard)?;
                Ok::<_, ClipboardError>(Worker {
                    x,
                    clipboard,
                    shared: owner,
                    last_owner,
                    last_time: 0,
                })
            })();
            match setup {
                Err(e) => {
                    let _ = started.send(Err(e));
                }
                Ok(mut worker) => {
                    if started.send(Ok(())).is_err() {
                        return;
                    }
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(5)) {
                            Ok(Command::Call(f)) => {
                                let _ = worker.drain();
                                f(&mut worker);
                            }
                            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        if let Err(e) = worker.drain() {
                            tracing::warn!(reason = %e, "X11 clipboard event failed");
                        }
                    }
                }
            }
        })
        .map_err(error)?;
    ready.recv().map_err(error)??;
    Ok(Box::new(Backend { handle: Handle(shared) }))
}
impl Worker {
    fn changed(&mut self) {
        self.shared.count.fetch_add(1, Ordering::AcqRel);
        if !self.shared.paused.load(Ordering::Acquire) {
            for callback in self.shared.handlers.lock().unwrap().clone() {
                callback();
            }
        }
    }
    fn drain(&mut self) -> Result<(), ClipboardError> {
        while let Some(event) = self.x.next()? {
            if let Event::XfixesSelectionNotify(event) = &event {
                if event.selection == self.clipboard
                    && (event.owner != self.last_owner || event.selection_timestamp != self.last_time)
                {
                    self.last_owner = event.owner;
                    self.last_time = event.selection_timestamp;
                    self.x.timestamps.insert(self.clipboard, event.selection_timestamp);
                    if event.owner != self.x.window {
                        self.changed();
                    }
                }
            }
            // A stale/malformed requester must not terminate our clipboard owner.
            if let Err(e) = self.x.service(&event) {
                tracing::debug!(reason = %e, "X11 selection requester retired");
            }
        }
        Ok(())
    }
    fn publish(
        &mut self,
        formats: Formats,
        lazy: x11::LazyFormats,
        guard: Option<content::ReaderGuard>,
    ) -> Result<u64, ClipboardError> {
        if let Some(guard) = guard {
            guard().map_err(|_| ClipboardError::Busy)?;
        }
        self.x
            .publish_lazy(self.clipboard, formats, lazy, x11rb::CURRENT_TIME)?;
        self.last_owner = self.x.window;
        self.changed();
        Ok(self.shared.count.load(Ordering::Acquire))
    }
}
impl ClipboardBackend for Backend {
    fn change_count(&self) -> u64 {
        self.handle.0.count.load(Ordering::Acquire)
    }
    fn clipboard_key(&self) -> io::Result<[u8; 32]> {
        let session = self.handle.0.session;
        self.handle
            .call(move |worker| {
                let mut hash = blake3::Hasher::new();
                hash.update(&session);
                hash.update(&worker.x.owner(worker.clipboard)?.to_le_bytes());
                hash.update(&worker.last_time.to_le_bytes());
                hash.update(&worker.shared.count.load(Ordering::Acquire).to_le_bytes());
                Ok(*hash.finalize().as_bytes())
            })
            .map_err(io_error)
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.handle.call(|worker| {
            let before = worker.shared.count.load(Ordering::Acquire);
            let items = payload::read(&mut worker.x, worker.clipboard, x11rb::CURRENT_TIME)?;
            let offered = worker.x.offered(worker.clipboard, x11rb::CURRENT_TIME)?;
            let hint = worker.x.atom("x-kde-passwordManagerHint")?;
            let sensitivity = if offered.contains(&hint)
                && worker
                    .x
                    .load(worker.clipboard, hint, x11rb::CURRENT_TIME)?
                    .starts_with(b"secret")
            {
                Sensitivity::Concealed
            } else {
                Sensitivity::Normal
            };
            worker.drain()?;
            if before != worker.shared.count.load(Ordering::Acquire) {
                return Err(ClipboardError::Busy);
            }
            Ok(ClipSnapshot {
                items,
                sensitivity,
                change_count: worker.shared.count.load(Ordering::Acquire),
            })
        })
    }
    fn capture(&self, _data: &std::path::Path) -> Result<CapturedClipboard, ClipboardError> {
        let (generation, formats, paths, sensitivity) = self.handle.call(|worker| {
            let before = worker.shared.count.load(Ordering::Acquire);
            let offered = worker.x.offered(worker.clipboard, x11rb::CURRENT_TIME)?;
            let formats = payload::available(&worker.x, &offered)?;
            let uri = worker.x.atom("text/uri-list")?;
            let gnome = worker.x.atom("x-special/gnome-copied-files")?;
            let paths = if offered.contains(&uri) {
                payload::decode_uris(&worker.x.load(worker.clipboard, uri, x11rb::CURRENT_TIME)?, false)?
            } else if offered.contains(&gnome) {
                payload::decode_uris(&worker.x.load(worker.clipboard, gnome, x11rb::CURRENT_TIME)?, true)?
            } else {
                Vec::new()
            };
            let hint = worker.x.atom("x-kde-passwordManagerHint")?;
            let sensitivity = if offered.contains(&hint)
                && worker.x.load(worker.clipboard, hint, x11rb::CURRENT_TIME)?.as_slice() == b"secret"
            {
                Sensitivity::Concealed
            } else {
                Sensitivity::Normal
            };
            worker.drain()?;
            if before != worker.shared.count.load(Ordering::Acquire) {
                return Err(ClipboardError::Busy);
            }
            Ok((worker.shared.count.load(Ordering::Acquire), formats, paths, sensitivity))
        })?;
        let namespace: Option<Arc<dyn Namespace>> = if paths.is_empty() {
            None
        } else {
            Some(
                content::namespace::Local::clipboard(
                    paths
                        .into_iter()
                        .enumerate()
                        .map(|(i, p)| (NodeId(i as u64 + 1), p))
                        .collect(),
                )
                .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?,
            )
        };
        let roots = namespace.as_ref().map_or_else(Vec::new, |n| n.roots());
        // A native file selection's text/image companions describe paths or
        // icons. Linux has no opaque reconstruction contract for these; keep
        // only the actual file namespace, including in a no-files projection.
        let reps = if roots.is_empty() {
            formats
                .iter()
                .enumerate()
                .map(|(i, f)| Representation {
                    id: RepresentationId(i as u32 + 1),
                    format: NativeFormat::Unknown(crate::negotiation::portable_format(f.standard).into()),
                    medium: Medium::Bytes,
                    aspect: 0,
                    index: -1,
                    standard: Some(f.standard),
                    data: RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    },
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut items = Vec::new();
        if !reps.is_empty() {
            items.push(ContentItem {
                id: ItemId(1),
                virtual_files: false,
                file: roots.first().map(|r| r.id),
                representations: reps,
            });
        }
        for (i, root) in roots.iter().enumerate() {
            if i == 0 && !items.is_empty() {
                continue;
            }
            items.push(ContentItem {
                id: ItemId(i as u32 + 2),
                virtual_files: false,
                file: Some(root.id),
                representations: Vec::new(),
            });
        }
        let inventory = Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: oink_foundation::DeviceId([0; 32]),
                    seq: 0,
                },
                generation,
            },
            platform: oink_foundation::clipboard::ClipPlatform::Unknown,
            items,
            roots,
        };
        let snapshot = ClipSnapshot {
            items: content::items(&inventory),
            sensitivity,
            change_count: generation,
        };
        let reader = self.handle.clone();
        let check_handle = self.handle.clone();
        let check: content::ReaderGuard = Arc::new(move || {
            if check_handle.0.count.load(Ordering::Acquire) == generation {
                Ok(())
            } else {
                Err(content::unavailable(Unavailable::Superseded))
            }
        });
        let read_check = check.clone();
        let provider = content::clipboard::Clipboard::new(
            inventory,
            Box::new(move |id| {
                let format = formats
                    .get(
                        id.0.checked_sub(1)
                            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?
                            as usize,
                    )
                    .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
                let mut bytes = reader.load(generation, format.atom)?;
                if format.latin1 {
                    bytes = bytes.iter().map(|&b| char::from(b)).collect::<String>().into_bytes();
                }
                content::guarded(Box::new(io::Cursor::new(bytes)), read_check.clone())
            }),
            check,
            Arc::new(payload::Translator),
            namespace,
        )
        .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
        Ok(CapturedClipboard {
            snapshot,
            content: Some(provider),
        })
    }
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        let items = items.to_vec();
        self.handle.call(move |worker| {
            let formats = payload::encode(&worker.x, &items, sensitivity)?;
            worker.publish(formats, x11::LazyFormats::new(), None)
        })
    }
    fn write_native(&self, spec: NativeSpec) -> Result<u64, ClipboardError> {
        self.write_native_guarded(spec, Arc::new(|| Ok(())))
    }
    fn write_native_guarded(&self, spec: NativeSpec, guard: content::ReaderGuard) -> Result<u64, ClipboardError> {
        self.write_native_cancellable(spec, guard, Arc::new(|| Ok(())))
    }
    fn write_native_cancellable(
        &self,
        spec: NativeSpec,
        guard: content::ReaderGuard,
        current: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        guard().map_err(|_| ClipboardError::Busy)?;
        current().map_err(|_| ClipboardError::Busy)?;
        let mut publication = payload::Publication::native(&spec, current.clone())?;
        publication.prefetch_files()?;
        current().map_err(|_| ClipboardError::Busy)?;
        self.handle.call(move |worker| {
            let (formats, lazy) = publication.formats(&worker.x, spec.sensitivity)?;
            worker.publish(formats, lazy, Some(guard))
        })
    }
    fn write_virtual_files(
        &self,
        _: &[crate::FileEntry],
        _: &[ClipData],
        _: Arc<dyn crate::FileContents>,
        _: Sensitivity,
    ) -> Result<u64, ClipboardError> {
        Err(ClipboardError::FileProviderUnavailable)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.handle.0.handlers.lock().unwrap().push(handler);
        Ok(())
    }
    fn set_watch_paused(&self, paused: bool) -> Result<(), ClipboardError> {
        self.handle.0.paused.store(paused, Ordering::Release);
        Ok(())
    }
}
