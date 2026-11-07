//! A private capture bus separates monitor PCM from local playback without changing defaults.
//! A durable route journal precedes every move; cleanup only moves streams still on our bus.
use super::*;
use crate::microphone::linux::pactl;
use crate::recovery;
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, sync::Condvar};
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Route {
    index: u32,
    original: String,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Record {
    version: u32,
    original: String,
    bus: String,
    module: Option<u32>,
    routes: Vec<Route>,
    #[serde(default)]
    revoked: Vec<u32>,
}
fn json(args: &[&str]) -> Result<Vec<serde_json::Value>, String> {
    serde_json::from_str(&pactl(args)?).map_err(|e| e.to_string())
}
fn streams() -> Result<Vec<serde_json::Value>, String> {
    json(&["--format=json", "list", "sink-inputs"])
}
fn defaults() -> Result<(String, String), String> {
    let output = devices()?
        .into_iter()
        .find(|d| d.is_default != 0)
        .ok_or("default output unavailable")?;
    let input = crate::microphone::linux::default_input()?.ok_or("default input unavailable")?;
    Ok((text(&output.name), input.id))
}
fn index(value: &serde_json::Value, key: &str) -> Option<u32> {
    value[key].as_u64().and_then(|i| u32::try_from(i).ok())
}
fn persist(dir: &Path, record: &Record) -> Result<(), String> {
    let file = dir.join("pulse-routing.json");
    let temp = dir.join(format!(".pulse-routing.{}.tmp", std::process::id()));
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    out.write_all(&serde_json::to_vec(record).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    out.sync_all().map_err(|e| e.to_string())?;
    fs::rename(temp, file).map_err(|e| e.to_string())
}
fn lock(dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("pulse-routing.lock"))
        .map_err(|e| e.to_string())?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| "another PulseAudio capture route owns the journal")?;
    Ok(file)
}
fn saved(dir: &Path) -> Result<Option<Record>, String> {
    match fs::read(dir.join("pulse-routing.json")) {
        Ok(bytes) => {
            let record: Record = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if record.version != 1
                || !record.bus.starts_with("oink_capture_")
                || record.bus.contains(char::is_whitespace)
                || record.original.is_empty()
            {
                return Err("invalid PulseAudio route journal; no routes changed".into());
            }
            Ok(Some(record))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn owned_module(record: &Record) -> Result<Option<u32>, String> {
    let list = super::modules()?;
    let token = format!("sink_name={}", record.bus);
    let matches: Vec<_> = list
        .iter()
        .filter(|m| {
            m["name"].as_str() == Some("module-null-sink")
                && m["argument"]
                    .as_str()
                    .is_some_and(|a| a.split_whitespace().any(|v| v == token))
        })
        .collect();
    if matches.len() > 1 {
        return Err("ambiguous private capture modules".into());
    }
    let found = matches.first().and_then(|m| index(m, "index"));
    if record.module.is_some() && found.is_some() && record.module != found {
        return Err("private capture module ownership changed".into());
    }
    Ok(found)
}
fn restore(dir: &Path, record: &Record) -> Result<bool, String> {
    let inventory = devices()?;
    let bus = inventory.iter().find(|d| text(&d.name) == record.bus).map(|d| d.index);
    let mut user_override = false;
    if let Some(bus) = bus {
        let current = streams()?;
        for route in &record.routes {
            if let Some(stream) = current.iter().find(|s| index(s, "index") == Some(route.index)) {
                if index(stream, "sink") == Some(bus) {
                    if !inventory.iter().any(|d| text(&d.name) == route.original) {
                        return Err("original output unavailable; capture route restoration is pending".into());
                    }
                    pactl(&["move-sink-input", &route.index.to_string(), &route.original])?;
                } else {
                    user_override = true;
                }
            }
        }
        if streams()?.iter().any(|s| index(s, "sink") == Some(bus)) {
            return Err("private capture bus has externally owned streams; leaving their routes untouched".into());
        }
    }
    if let Some(module) = owned_module(record)? {
        pactl(&["unload-module", &module.to_string()])?;
    }
    match fs::remove_file(dir.join("pulse-routing.json")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    crate::device_notifications::invalidate();
    Ok(user_override)
}
pub(super) fn recover(dir: &Path) -> Result<Option<RecoveryReport>, String> {
    let _lock = lock(dir)?;
    let Some(record) = saved(dir)? else { return Ok(None) };
    let user = restore(dir, &record)?;
    Ok(Some(RecoveryReport {
        outcome: if user {
            recovery::RecoveryOutcome::UserOverride
        } else {
            recovery::RecoveryOutcome::Restored
        },
        endpoint: Some(format!("pulse:{}", record.original)),
        crash_intent_uncertain: true,
    }))
}
pub(super) struct Bus {
    dir: PathBuf,
    record: Record,
    _lock: fs::File,
    closed: bool,
}
impl Bus {
    pub fn create(dir: &Path, original: &str) -> Result<Self, String> {
        let guard = lock(dir)?;
        if let Some(record) = saved(dir)? {
            restore(dir, &record)?;
        }
        let before = defaults()?;
        if before.0 != original {
            return Err("default output changed before private capture setup".into());
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let record = Record {
            version: 1,
            original: original.into(),
            bus: format!("oink_capture_{}_{nonce:x}", std::process::id()),
            module: None,
            routes: vec![],
            revoked: vec![],
        };
        persist(dir, &record)?;
        let mut bus = Self {
            dir: dir.into(),
            record,
            _lock: guard,
            closed: false,
        };
        let rate = devices()?
            .iter()
            .find(|d| text(&d.name) == original)
            .ok_or("source output unavailable")?
            .rate;
        let args = format!(
            "sink_name={} rate={rate} channels=2 sink_properties=device.description=Oink_Private_Capture",
            bus.record.bus
        );
        let module = pactl(&["load-module", "module-null-sink", &args])?
            .trim()
            .parse::<u32>()
            .map_err(|e| e.to_string())?;
        bus.record.module = Some(module);
        persist(dir, &bus.record)?;
        if defaults()? != before {
            return Err(
                "audio server changed a default device while adding the private capture bus; setup rolled back".into(),
            );
        }
        Ok(bus)
    }
    pub fn name(&self) -> &str {
        &self.record.bus
    }
    fn sync(&mut self) -> Result<(), String> {
        let inventory = devices()?;
        let bus = inventory
            .iter()
            .find(|d| text(&d.name) == self.record.bus)
            .ok_or("private capture bus disappeared")?
            .index;
        let original = inventory
            .iter()
            .find(|d| text(&d.name) == self.record.original)
            .ok_or("source output disappeared")?
            .index;
        let current = streams()?;
        // Once a user moves a stream elsewhere, relinquish that stream, including moves back later.
        let changed: Vec<u32> = self
            .record
            .routes
            .iter()
            .filter(|r| {
                current
                    .iter()
                    .find(|s| index(s, "index") == Some(r.index))
                    .is_some_and(|s| index(s, "sink") != Some(bus))
            })
            .map(|r| r.index)
            .collect();
        for id in &changed {
            if !self.record.revoked.contains(id) {
                self.record.revoked.push(*id);
            }
        }
        let previous = self.record.routes.len();
        self.record.routes.retain(|r| {
            current
                .iter()
                .find(|s| index(s, "index") == Some(r.index))
                .is_some_and(|s| index(s, "sink") == Some(bus))
        });
        if previous != self.record.routes.len() {
            persist(&self.dir, &self.record)?;
        }
        if !changed.is_empty() {
            return Err("application audio routing changed externally; preserving user routes".into());
        }
        for stream in current {
            let Some(id) = index(&stream, "index") else { continue };
            if index(&stream, "sink") != Some(original)
                || stream["properties"]["application.name"].as_str() == Some("Oink")
            {
                continue;
            }
            if self.record.revoked.contains(&id) || self.record.routes.iter().any(|r| r.index == id) {
                continue;
            }
            self.record.routes.push(Route {
                index: id,
                original: self.record.original.clone(),
            });
            persist(&self.dir, &self.record)?;
            pactl(&["move-sink-input", &id.to_string(), &self.record.bus])
                .map_err(|e| format!("cannot redirect application audio without changing defaults: {e}"))?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        restore(&self.dir, &self.record)?;
        self.closed = true;
        Ok(())
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::warn!(%error,"private capture route recovery remains pending");
        }
    }
}
pub(super) struct Router {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
    cleanup: Arc<Mutex<Option<String>>>,
}
impl Router {
    pub fn start(mut bus: Bus, mut pump: impl FnMut() -> Result<(), String>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let error = Arc::new(Mutex::new(None));
        let cleanup = Arc::new(Mutex::new(None));
        let (s, w, e, c) = (stop.clone(), wake.clone(), error.clone(), cleanup.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-pulse-routing".into())
            .spawn(move || {
                let notify = w.clone();
                let _subscription = crate::device_notifications::subscribe(Arc::new(move || {
                    *notify.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    notify.1.notify_one();
                }));
                match bus.sync() {
                    Ok(()) => {
                        let _ = tx.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.clone()));
                        *e.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                        if let Err(error) = bus.finish() {
                            *c.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                        }
                        return;
                    }
                }
                while !s.load(Ordering::Acquire) {
                    let changed = w.0.lock().unwrap_or_else(|e| e.into_inner());
                    let (mut changed, _) =
                        w.1.wait_timeout_while(changed, Duration::from_secs(1), |changed| {
                            !*changed && !s.load(Ordering::Acquire)
                        })
                        .unwrap_or_else(|e| e.into_inner());
                    *changed = false;
                    drop(changed);
                    if s.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = bus.sync() {
                        *e.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                        break;
                    }
                }
                if let Err(error) = bus.finish() {
                    *c.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                }
            })
            .map_err(|e| e.to_string())?;
        loop {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    return Ok(Self {
                        stop,
                        wake,
                        worker: Some(worker),
                        error,
                        cleanup,
                    });
                }
                Ok(Err(error)) => {
                    let _ = worker.join();
                    return Err(error);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = worker.join();
                    return Err("private audio router stopped during setup".into());
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if let Err(error) = pump() {
                        stop.store(true, Ordering::Release);
                        *wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                        wake.1.notify_all();
                        let _ = worker.join();
                        return Err(error);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn finish(&mut self) -> Option<String> {
        self.stop.store(true, Ordering::Release);
        *self.wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.1.notify_all();
        if let Some(w) = self.worker.take() {
            if w.join().is_err() {
                return Some("private audio router panicked".into());
            }
        }
        self.cleanup.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
