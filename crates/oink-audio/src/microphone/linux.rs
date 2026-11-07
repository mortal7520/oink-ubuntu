//! PulseAudio hardware capture and explicit, user-session virtual microphone modules.
use super::*;
use crate::linux::{CaptureCallback, Device, Native, captured, oink_pa_iterate, oink_pa_stream, text};
use std::os::unix::fs::OpenOptionsExt;
use std::{
    ffi::{CString, c_char, c_int},
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const FEED: &str = "oink_microphone_feed";
const INPUT: &str = "oink_microphone";
const SINK_ARGS: &str = "sink_name=oink_microphone_feed sink_properties=device.description=Oink_Microphone_Feed";
const SOURCE_ARGS: &str = "master=oink_microphone_feed.monitor source_name=oink_microphone source_properties=device.description=Oink_Microphone";
unsafe extern "C" {
    fn oink_pa_sources(out: *mut Device, cap: usize, error: *mut c_char, size: usize) -> c_int;
    fn oink_pa_consumers(out: *mut u32, cap: usize, error: *mut c_char, size: usize) -> c_int;
}
fn sources() -> Result<Vec<Device>, String> {
    let mut values = [Device::default(); 64];
    let mut e = [0; 512];
    // SAFETY: C receives two writable fixed-capacity buffers and retains neither.
    let n = unsafe { oink_pa_sources(values.as_mut_ptr(), values.len(), e.as_mut_ptr(), e.len()) };
    if n < 0 {
        Err(text(&e))
    } else {
        Ok(values[..n as usize].to_vec())
    }
}
fn descriptor(d: &Device) -> InputDevice {
    InputDevice {
        id: format!("pulse-source:{}", text(&d.name)),
        name: text(&d.description),
    }
}
pub fn inputs() -> Result<Vec<InputDevice>, String> {
    Ok(sources()?
        .iter()
        .filter(|d| text(&d.monitor) != "monitor")
        .map(descriptor)
        .collect())
}
pub fn default_input() -> Result<Option<InputDevice>, String> {
    Ok(sources()?.iter().find(|d| d.is_default != 0).map(descriptor))
}
pub fn input_candidates() -> Result<Vec<InputCandidate>, String> {
    Ok(sources()?
        .iter()
        .map(|d| {
            let name = text(&d.name);
            let kind = text(&d.monitor);
            InputCandidate {
                input: descriptor(d),
                physical: kind.is_empty(),
                built_in: kind.is_empty() && name.contains("pci"),
                forwarding_loopback: name == INPUT || kind == "monitor",
            }
        })
        .collect())
}
pub fn switch_default_input(_expected: &str, _replacement: &str) -> Result<(), String> {
    Err("select a hardware microphone explicitly; Oink does not change Linux default devices".into())
}
pub fn virtual_output() -> Result<VirtualOutput, String> {
    let output = crate::linux::outputs()?
        .into_iter()
        .find(|d| d.id == format!("pulse:{FEED}"));
    let source = sources()?.iter().any(|d| text(&d.name) == INPUT);
    Ok(match (output, source) {
        (Some(output), true) => VirtualOutput::Ready {
            render_id: output.id,
            recording_name: "Oink Microphone".into(),
        },
        _ => VirtualOutput::Missing,
    })
}
#[derive(Debug)]
pub struct Capture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
    info: CaptureInfo,
}
impl Capture {
    pub fn start(id: &str, permit: StartPermit, encoder: crate::source::Encoder) -> Result<Self, String> {
        permit.check()?;
        let raw = id
            .strip_prefix("pulse-source:")
            .filter(|s| !s.is_empty() && !s.contains('\0'))
            .ok_or("expected an exact PulseAudio source ID")?;
        let d = sources()?
            .into_iter()
            .find(|d| text(&d.name) == raw)
            .ok_or("selected microphone is unavailable")?;
        if raw == INPUT || text(&d.monitor) == "monitor" {
            return Err("select a hardware microphone, not a forwarding loopback".into());
        }
        let device = descriptor(&d);
        let name = CString::new(raw).map_err(|_| "invalid source ID")?;
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (s, e) = (stop.clone(), error.clone());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-pulse-microphone".into())
            .spawn(move || {
                let mut cb = Box::new(CaptureCallback {
                    encoder,
                    panicked: false,
                });
                let mut message = [0; 512];
                // SAFETY: callback is boxed for the complete lifetime of this worker-owned native input.
                let pointer = unsafe {
                    oink_pa_stream(
                        name.as_ptr(),
                        2,
                        48_000,
                        240,
                        captured,
                        (&mut *cb as *mut CaptureCallback).cast(),
                        message.as_mut_ptr(),
                        message.len(),
                    )
                };
                if pointer.is_null() {
                    let _ = tx.send(Err(text(&message)));
                    return;
                }
                let native = Native(pointer);
                let _ = tx.send(Ok(()));
                while !s.load(Ordering::Acquire) {
                    if unsafe { oink_pa_iterate(native.0) } < 0 || cb.panicked {
                        *e.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some("PulseAudio microphone disconnected or overflowed".into());
                        break;
                    }
                    cb.encoder.tick(Instant::now());
                    thread::sleep(Duration::from_millis(1));
                }
            })
            .map_err(|e| e.to_string())?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                let _ = worker.join();
                return Err("microphone worker stopped during startup".into());
            }
        }
        Ok(Self {
            stop,
            error,
            worker: Some(worker),
            info: CaptureInfo {
                device,
                backend: "pulseaudio_source_server_resampled",
                hardware_sample_rate: d.rate,
                hardware_channels: d.channels as u16,
                resampler_delay_frames: 0,
            },
        })
    }
    pub fn info(&self) -> &CaptureInfo {
        &self.info
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn device_invalidated(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished) && !self.stop.load(Ordering::Acquire)
    }
    pub fn stop_after_device_invalidation(&mut self) {
        self.stop();
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}
pub struct DemandMonitor {
    _wake: Arc<dyn Fn() + Send + Sync>,
    epoch: u64,
    consumers: Vec<u32>,
}
impl std::fmt::Debug for DemandMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulseMicrophoneDemand")
            .field("epoch", &self.epoch)
            .field("consumers", &self.consumers.len())
            .finish()
    }
}
impl DemandMonitor {
    pub fn new() -> Result<Self, String> {
        Self::with_wake(Arc::new(|| {}))
    }
    pub fn with_wake(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        if !matches!(virtual_output()?, VirtualOutput::Ready { .. }) {
            return Err("Oink virtual microphone is unavailable".into());
        }
        Ok(Self {
            _wake: crate::device_notifications::subscribe(wake),
            epoch: 0,
            consumers: vec![],
        })
    }
    pub fn revision(&self) -> u64 {
        crate::linux::consumer_generation()
    }
    pub fn sample(&mut self) -> Demand {
        let mut ids = [0; 128];
        let mut e = [0; 512];
        // SAFETY: metadata query only, with writable bounded buffers; no capture is opened.
        let count = unsafe { oink_pa_consumers(ids.as_mut_ptr(), ids.len(), e.as_mut_ptr(), e.len()) };
        if count < 0 {
            return Demand {
                active: false,
                epoch: self.epoch,
                error: Some(text(&e)),
            };
        }
        let mut consumers = ids[..count as usize].to_vec();
        consumers.sort_unstable();
        // Pulse-compatible servers can emit source-output metadata changes without
        // changing consumer lifetimes. Notifications invalidate the caller cache;
        // only the observed uncorked consumer set advances the forwarding epoch.
        if consumers != self.consumers {
            self.epoch = self.epoch.wrapping_add(1).max(1);
            self.consumers = consumers;
        }
        Demand {
            active: !self.consumers.is_empty(),
            epoch: self.epoch,
            error: None,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Receipt {
    version: u32,
    sink: u32,
    source: u32,
}
fn state_dir() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(value);
        if p.is_absolute() {
            return Ok(p.join("oink"));
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(".local/state/oink"))
        .ok_or_else(|| "no user state directory".into())
}
fn receipt() -> Result<Option<Receipt>, String> {
    match fs::read(state_dir()?.join("linux-microphone.json")) {
        Ok(bytes) => serde_json::from_slice::<Receipt>(&bytes)
            .map(Some)
            .map_err(|e| format!("invalid virtual microphone receipt: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub(crate) fn pactl(args: &[&str]) -> Result<String, String> {
    let mut child = std::process::Command::new("pactl")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("PulseAudio tools are required: {e}"))?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let stdout = thread::spawn(move || {
        let mut bytes = vec![];
        let _ = out.take(1_048_576).read_to_end(&mut bytes);
        bytes
    });
    let stderr = thread::spawn(move || {
        let mut bytes = vec![];
        let _ = err.take(65536).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    match status {
        Some(s) if s.success() => String::from_utf8(out).map_err(|e| e.to_string()),
        Some(_) => Err(format!(
            "PulseAudio operation failed: {}",
            String::from_utf8_lossy(&err).trim()
        )),
        None => Err("PulseAudio operation timed out".into()),
    }
}
fn modules() -> Result<Vec<serde_json::Value>, String> {
    crate::linux::modules()
}
fn owned_module(modules: &[serde_json::Value], index: u32, name: &str, args: &str) -> bool {
    modules.iter().any(|m| {
        m["index"].as_u64() == Some(index as u64)
            && m["name"].as_str() == Some(name)
            && m["argument"].as_str().is_some_and(|a| {
                args.split_whitespace()
                    .all(|required| a.split_whitespace().any(|v| v == required))
            })
    })
}
fn owned(r: &Receipt, list: &[serde_json::Value]) -> bool {
    r.version == 1
        && owned_module(list, r.sink, "module-null-sink", SINK_ARGS)
        && owned_module(list, r.source, "module-remap-source", SOURCE_ARGS)
}
pub fn virtual_installation() -> Result<(bool, VirtualOutput), String> {
    let output = virtual_output()?;
    let managed = match receipt()? {
        Some(r) => owned(&r, &modules()?),
        None => false,
    };
    Ok((managed, output))
}
fn defaults() -> Result<(String, String), String> {
    let output = crate::linux::outputs()?
        .into_iter()
        .find(|d| d.is_default)
        .ok_or("no default output; cannot install without changing defaults")?
        .id;
    let input = default_input()?
        .ok_or("no default input; cannot install without changing defaults")?
        .id;
    Ok((output, input))
}
fn transaction_lock() -> Result<fs::File, String> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("linux-microphone.lock"))
        .map_err(|e| e.to_string())?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| "another virtual microphone operation is active")?;
    Ok(file)
}
pub fn install_virtual() -> Result<(), String> {
    let _lock = transaction_lock()?;
    let (managed, output) = virtual_installation()?;
    if managed && matches!(output, VirtualOutput::Ready { .. }) {
        return Ok(());
    }
    if matches!(output, VirtualOutput::Ready { .. }) {
        return Err("Oink-named virtual devices exist without a matching ownership receipt".into());
    }
    if crate::linux::outputs()?.iter().any(|d| d.id == format!("pulse:{FEED}"))
        || sources()?.iter().any(|d| text(&d.name) == INPUT)
    {
        return Err("partial Oink-named virtual devices already exist; no foreign modules changed".into());
    }
    let before = defaults()?;
    let sink = pactl(&["load-module", "module-null-sink", SINK_ARGS])?
        .trim()
        .parse::<u32>()
        .map_err(|_| "invalid PulseAudio module ID")?;
    let source = match pactl(&["load-module", "module-remap-source", SOURCE_ARGS])
        .and_then(|s| s.trim().parse::<u32>().map_err(|e| e.to_string()))
    {
        Ok(id) => id,
        Err(e) => {
            let _ = pactl(&["unload-module", &sink.to_string()]);
            return Err(e);
        }
    };
    let r = Receipt {
        version: 1,
        sink,
        source,
    };
    let result = (|| {
        if defaults()? != before {
            return Err("audio server changed a default device; virtual microphone installation rolled back".into());
        }
        if !matches!(virtual_output()?, VirtualOutput::Ready { .. }) {
            return Err("virtual microphone modules did not expose both endpoints".into());
        }
        let file = state_dir()?.join("linux-microphone.json");
        let temp = file.with_extension(format!("{}.tmp", std::process::id()));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        f.write_all(&serde_json::to_vec(&r).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, &file).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = pactl(&["unload-module", &source.to_string()]);
        let _ = pactl(&["unload-module", &sink.to_string()]);
    }
    crate::device_notifications::invalidate();
    result
}
pub fn uninstall_virtual() -> Result<(), String> {
    let _lock = transaction_lock()?;
    let Some(r) = receipt()? else {
        return Err("no Oink virtual microphone ownership receipt".into());
    };
    let list = modules()?;
    for (index, name, args) in [
        (r.source, "module-remap-source", SOURCE_ARGS),
        (r.sink, "module-null-sink", SINK_ARGS),
    ] {
        if list.iter().any(|m| m["index"].as_u64() == Some(index as u64)) && !owned_module(&list, index, name, args) {
            return Err("virtual microphone ownership no longer matches the audio server; no modules removed".into());
        }
    }
    for index in [r.source, r.sink] {
        if list.iter().any(|m| m["index"].as_u64() == Some(index as u64)) {
            pactl(&["unload-module", &index.to_string()])?;
        }
    }
    fs::remove_file(state_dir()?.join("linux-microphone.json")).map_err(|e| e.to_string())?;
    crate::device_notifications::invalidate();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_requires_exact_module_ids_names_and_arguments() {
        let modules = vec![
            serde_json::json!({"index":7,"name":"module-null-sink","argument":SINK_ARGS}),
            serde_json::json!({"index":8,"name":"module-remap-source","argument":SOURCE_ARGS}),
        ];
        assert!(owned(
            &Receipt {
                version: 1,
                sink: 7,
                source: 8
            },
            &modules
        ));
        assert!(!owned(
            &Receipt {
                version: 1,
                sink: 8,
                source: 7
            },
            &modules
        ));
        assert!(!owned_module(
            &modules,
            7,
            "module-null-sink",
            "sink_name=oink_microphone"
        ));
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    struct Tone(f32);
    impl crate::playback::NativeRender for Tone {
        fn fill(&mut self, out: &mut [f32], _: crate::linux::NativeOutputTiming) {
            for pair in out.chunks_mut(2) {
                let sample = self.0.sin() * 0.1;
                self.0 = (self.0 + std::f32::consts::TAU * 440.0 / 48_000.0) % std::f32::consts::TAU;
                pair.fill(sample);
            }
        }
        fn complete(&mut self, _: Duration) {}
    }
    struct Consumer {
        child: std::process::Child,
        reader: Option<JoinHandle<()>>,
        nonzero: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Consumer {
        fn start() -> Self {
            let mut child = std::process::Command::new("parec")
                .args([
                    "--raw",
                    "--device=oink_microphone",
                    "--format=float32le",
                    "--rate=48000",
                    "--channels=2",
                    "--latency-msec=20",
                    "--process-time-msec=5",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut output = child.stdout.take().unwrap();
            let nonzero = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let count = nonzero.clone();
            let reader = thread::spawn(move || {
                let mut pending = Vec::with_capacity(8196);
                let mut buffer = [0; 8192];
                loop {
                    let Ok(size) = output.read(&mut buffer) else { break };
                    if size == 0 {
                        break;
                    }
                    pending.extend_from_slice(&buffer[..size]);
                    let complete = pending.len() / 4 * 4;
                    let values = pending[..complete]
                        .chunks_exact(4)
                        .filter(|bytes| f32::from_le_bytes((*bytes).try_into().unwrap()).abs() > 0.01)
                        .count();
                    count.fetch_add(values as u64, Ordering::Relaxed);
                    pending.drain(..complete);
                }
            });
            Self {
                child,
                reader: Some(reader),
                nonzero,
            }
        }
        fn stop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
    impl Drop for Consumer {
        fn drop(&mut self) {
            self.stop();
        }
    }
    #[test]
    #[ignore = "requires isolated PulseAudio with oink_test_output and isolated XDG_STATE_HOME"]
    fn virtual_microphone_has_real_consumer_demand_and_owned_cleanup() {
        assert_eq!(std::env::var("OINK_TEST_PULSE").as_deref(), Ok("1"));
        assert!(std::env::var("PULSE_SERVER").is_ok_and(|s| s.starts_with("unix:")));
        assert!(std::env::var("XDG_STATE_HOME").is_ok_and(|s| s.contains("oink")));
        let before = defaults().unwrap();
        assert_eq!(before.0, "pulse:oink_test_output");
        install_virtual().unwrap();
        let (managed, output) = virtual_installation().unwrap();
        assert!(managed, "native module IDs and ownership receipt agree");
        assert!(matches!(output, VirtualOutput::Ready { .. }));
        assert_eq!(defaults().unwrap(), before);
        let mut monitor = DemandMonitor::new().unwrap();
        assert!(!monitor.sample().active);
        let mut consumer = Consumer::start();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut active = false;
        while Instant::now() < deadline {
            if monitor.sample().active {
                active = true;
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        assert!(active, "real recording consumer must create demand");
        let mut feed =
            crate::linux::NativePlayback::start(FEED, Some(240), Box::new(Tone(0.0)), Box::new(|_| {})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while consumer.nonzero.load(Ordering::Acquire) < 1000 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            consumer.nonzero.load(Ordering::Acquire) > 1000,
            "native playback must reach the virtual recording source as nonzero PCM"
        );
        thread::sleep(Duration::from_millis(100));
        let baseline = monitor.sample();
        let baseline_consumers = monitor.consumers.clone();
        let mut volume = crate::linux::open_volume("pulse:oink_test_output").unwrap();
        let original = volume.read().unwrap();
        volume
            .write(crate::volume::Reading {
                percent: 80,
                muted: original.muted,
            })
            .unwrap();
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            monitor.sample().epoch,
            baseline.epoch,
            "speaker volume cannot restart microphone consumers (before={baseline_consumers:?}, after={:?})",
            monitor.consumers
        );
        volume.write(original).unwrap();
        assert!(feed.shutdown());
        consumer.stop();
        let deadline = Instant::now() + Duration::from_secs(2);
        while monitor.sample().active && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!monitor.sample().active);
        // Exercise the physical-input API against a synthetic source, never the user's microphone.
        let fixture = pactl(&[
            "load-module",
            "module-remap-source",
            "master=oink_test_output.monitor source_name=oink_test_hardware rate=48000 channels=2",
        ])
        .unwrap();
        let (encoder, mut encoded) = crate::source::channel(crate::dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let mut capture = Capture::start(
            "pulse-source:oink_test_hardware",
            StartPermit {
                user_enabled: true,
                receiver_has_consumer: true,
            },
            encoder,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while encoded.frames() == 0 && Instant::now() < deadline {
            while encoded.try_recv().is_some() {}
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            encoded.frames() > 0,
            "the selected native microphone source must deliver actual frames"
        );
        assert_eq!(capture.info().device.id, "pulse-source:oink_test_hardware");
        assert!(capture.error().is_none());
        capture.stop();
        pactl(&["unload-module", fixture.trim()]).unwrap();
        uninstall_virtual().unwrap();
        assert!(matches!(virtual_output().unwrap(), VirtualOutput::Missing));
        assert_eq!(defaults().unwrap(), before);
    }
}
