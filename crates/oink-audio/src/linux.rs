//! Native PulseAudio sink monitors and output streams (including pipewire-pulse).
//! No default devices are changed. Native objects are confined to one worker.
#[path = "linux_route.rs"]
mod routes;
use crate::{
    CaptureInfo, CaptureScope, OutputDevice,
    recovery::{EndpointBackend, EndpointState, Journal, RecoveryReport},
    source,
};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Device {
    pub name: [c_char; 512],
    pub description: [c_char; 512],
    pub monitor: [c_char; 512],
    pub rate: u32,
    pub channels: u32,
    pub index: u32,
    pub volume: u32,
    pub muted: c_int,
    pub is_default: c_int,
}
impl Default for Device {
    fn default() -> Self {
        Self {
            name: [0; 512],
            description: [0; 512],
            monitor: [0; 512],
            rate: 0,
            channels: 0,
            index: 0,
            volume: 0,
            muted: 0,
            is_default: 0,
        }
    }
}
pub(crate) fn text(value: &[c_char]) -> String {
    let bytes: Vec<u8> = value.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
pub(crate) type PCM = unsafe extern "C" fn(*mut c_void, *mut f32, usize, u64, c_int, c_int);
unsafe extern "C" {
    fn oink_pa_devices(out: *mut Device, cap: usize, error: *mut c_char, size: usize) -> c_int;
    pub(crate) fn oink_pa_stream(
        device: *const c_char,
        record: c_int,
        rate: u32,
        frames: u32,
        callback: PCM,
        user: *mut c_void,
        error: *mut c_char,
        size: usize,
    ) -> *mut c_void;
    fn oink_pa_close(stream: *mut c_void);
    pub(crate) fn oink_pa_iterate(stream: *mut c_void) -> c_int;
    fn oink_pa_playback_iterate(stream: *mut c_void) -> c_int;
    fn oink_pa_buffer(stream: *mut c_void) -> u32;
    fn oink_pa_watch_output(stream: *mut c_void, name: *const c_char) -> c_int;
    fn oink_pa_mute(name: *const c_char, mute: c_int, error: *mut c_char, size: usize) -> c_int;
    fn oink_pa_volume(name: *const c_char, volume: u32, error: *mut c_char, size: usize) -> c_int;
}
fn devices() -> Result<Vec<Device>, String> {
    let mut values = [Device::default(); 64];
    let mut error = [0; 512];
    // SAFETY: both buffers are writable for their advertised capacity; C retains neither.
    let count = unsafe { oink_pa_devices(values.as_mut_ptr(), values.len(), error.as_mut_ptr(), error.len()) };
    if count < 0 {
        return Err(text(&error));
    }
    Ok(values[..count as usize].to_vec())
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Module {
    index: u32,
    name: [c_char; 128],
    argument: [c_char; 2048],
}
pub(crate) fn modules() -> Result<Vec<serde_json::Value>, String> {
    unsafe extern "C" {
        fn oink_pa_modules(out: *mut Module, cap: usize, error: *mut c_char, size: usize) -> c_int;
    }
    let empty = Module {
        index: 0,
        name: [0; 128],
        argument: [0; 2048],
    };
    let mut values = vec![empty; 128];
    let mut e = [0; 512];
    // SAFETY: native metadata writes fit the two provided buffers; no identifiers or arguments are logged.
    let count = unsafe { oink_pa_modules(values.as_mut_ptr(), values.len(), e.as_mut_ptr(), e.len()) };
    if count < 0 {
        return Err(text(&e));
    }
    Ok(values[..count as usize]
        .iter()
        .map(|m| serde_json::json!({"index":m.index,"name":text(&m.name),"argument":text(&m.argument)}))
        .collect())
}

fn raw_id(id: &str) -> Result<&str, String> {
    id.strip_prefix("pulse:")
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| "expected an exact PulseAudio output ID".into())
}
pub(crate) fn outputs() -> Result<Vec<OutputDevice>, String> {
    Ok(devices()?
        .into_iter()
        .filter(|d| !text(&d.name).starts_with("oink_capture_"))
        .map(|d| OutputDevice {
            id: format!("pulse:{}", text(&d.name)),
            name: text(&d.description),
            is_default: d.is_default != 0,
        })
        .collect())
}
pub(crate) fn capture_sample_rate() -> Result<u32, String> {
    let output = device(None)?;
    if !(8_000..=192_000).contains(&output.rate) {
        return Err("PulseAudio default-output rate is outside supported capture limits".into());
    }
    Ok(output.rate)
}
fn device(id: Option<&str>) -> Result<Device, String> {
    let name = id.map(raw_id).transpose()?;
    devices()?
        .into_iter()
        .find(|d| name.map_or(d.is_default != 0, |n| text(&d.name) == n))
        .ok_or_else(|| "selected PulseAudio output is unavailable".into())
}
pub fn endpoint_state(id: Option<&str>) -> Result<EndpointState, String> {
    let d = device(id)?;
    let volume = d.volume as f32 / 65_536.0;
    if volume > 1.0 {
        return Err("source volume above 100% cannot be safely journaled; lower it before forwarding".into());
    }
    Ok(EndpointState {
        endpoint: format!("pulse:{}", text(&d.name)),
        muted: d.muted != 0,
        volume,
    })
}
fn set_mute(id: &str, muted: bool) -> Result<(), String> {
    let name = CString::new(raw_id(id)?).map_err(|_| "invalid output ID")?;
    let mut e = [0; 512];
    // SAFETY: the NUL-terminated name and writable error buffer outlive this synchronous call.
    if unsafe { oink_pa_mute(name.as_ptr(), i32::from(muted), e.as_mut_ptr(), e.len()) } < 0 {
        Err(text(&e))
    } else {
        Ok(())
    }
}
#[derive(Default)]
struct Controls {
    lost: AtomicBool,
    mute_lost: AtomicBool,
}
impl EndpointBackend for Controls {
    fn read(&self, id: &str) -> Result<Option<EndpointState>, String> {
        let name = raw_id(id)?;
        let all = devices()?;
        let Some(d) = all.into_iter().find(|d| text(&d.name) == name) else {
            return Ok(None);
        };
        let volume = d.volume as f32 / 65_536.0;
        if volume > 1.0 {
            return Err("source volume exceeds journal range".into());
        }
        Ok(Some(EndpointState {
            endpoint: id.into(),
            muted: d.muted != 0,
            volume,
        }))
    }
    fn set_mute(&self, id: &str, value: bool) -> Result<(), String> {
        set_mute(id, value)
    }
    fn ownership_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
    fn mute_ownership_lost(&self) -> bool {
        self.mute_lost.load(Ordering::Acquire)
    }
}
pub fn recover(dir: &Path) -> Result<RecoveryReport, String> {
    let route_report = routes::recover(dir)?;
    let mut journal = Journal::open(dir)?;
    let controls = Controls::default();
    let endpoints = journal.endpoints();
    let mut result = journal.recover_or_defer(&controls, true)?;
    for id in endpoints {
        if journal.endpoints().iter().any(|saved| saved == &id) {
            journal.select_endpoint(&id)?;
            result = journal.recover_or_defer(&controls, true)?;
        }
    }
    Ok(route_report.unwrap_or(result))
}
pub(crate) struct Native(pub *mut c_void);
impl Drop for Native {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: only the creating worker owns this native object; callbacks have no other thread.
            unsafe { oink_pa_close(self.0) }
        }
    }
}
pub(crate) struct CaptureCallback {
    pub encoder: source::Encoder,
    pub panicked: bool,
}
pub(crate) unsafe extern "C" fn captured(
    user: *mut c_void,
    data: *mut f32,
    len: usize,
    latency: u64,
    known: c_int,
    _mode: c_int,
) {
    // SAFETY: user points to a live boxed callback on this same worker; C serializes callbacks.
    let state = unsafe { &mut *user.cast::<CaptureCallback>() };
    if state.panicked {
        return;
    }
    let at = Instant::now();
    let first = (known != 0)
        .then(|| at.checked_sub(Duration::from_micros(latency)))
        .flatten();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let silence = [0.0f32; 960];
        let mut offset = 0;
        while offset < len {
            let n = (len - offset).min(silence.len()) & !1;
            if n == 0 {
                break;
            }
            let samples = if data.is_null() {
                &silence[..n]
            } else {
                // SAFETY: PulseAudio peek storage contains len floats and remains valid until this callback returns.
                unsafe { std::slice::from_raw_parts(data.add(offset), n) }
            };
            state.encoder.supply(
                samples,
                source::Stamp {
                    first_sample_at: first.map(|t| {
                        t + Duration::from_secs_f64(offset as f64 / 2.0 / state.encoder.config().sample_rate as f64)
                    }),
                    callback_at: Some(at),
                    delivered_at: at,
                    source_frame_position: None,
                    discontinuity: false,
                },
            );
            offset += n;
        }
    }));
    if result.is_err() {
        state.panicked = true;
    }
}
#[derive(Debug)]
pub struct Capture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    cleanup: Arc<Mutex<Option<String>>>,
    changed: Arc<AtomicBool>,
    controls_changed: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    info: CaptureInfo,
}
impl Capture {
    pub fn start_encoded(encoder: source::Encoder, mute: bool, dir: PathBuf) -> Result<Self, String> {
        Self::start_encoded_with_scope(encoder, mute, dir, CaptureScope::DefaultOutput)
    }
    pub fn start_encoded_with_scope(
        encoder: source::Encoder,
        mute: bool,
        dir: PathBuf,
        scope: CaptureScope,
    ) -> Result<Self, String> {
        if scope == CaptureScope::Global
            && devices()?
                .iter()
                .filter(|d| {
                    let name = text(&d.name);
                    name != "oink_microphone_feed" && !name.starts_with("oink_capture_")
                })
                .count()
                != 1
        {
            return Err(
                "global capture across multiple PulseAudio outputs is not supported; select default output capture"
                    .into(),
            );
        }
        let selected = device(None)?;
        if text(&selected.name) == "oink_microphone_feed" {
            return Err("the default output is the virtual microphone feed; choose a system output".into());
        }
        let id = format!("pulse:{}", text(&selected.name));
        let name = CString::new(text(&selected.name)).map_err(|_| "invalid PulseAudio sink name")?;
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let cleanup = Arc::new(Mutex::new(None));
        let changed = Arc::new(AtomicBool::new(false));
        let controls_changed = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let (stop2, error2, cleanup2, changed2, controls2, muted2) = (
            stop.clone(),
            error.clone(),
            cleanup.clone(),
            changed.clone(),
            controls_changed.clone(),
            muted.clone(),
        );
        let (tx, rx) = mpsc::sync_channel(1);
        let worker=thread::Builder::new().name("oink-pulse-capture".into()).spawn(move||{
   let mut callback=Box::new(CaptureCallback{encoder,panicked:false});let mut native_error=[0;512];let rate=callback.encoder.config().sample_rate;
   let mut bus=if mute{match routes::Bus::create(&dir,&text(&selected.name)){Ok(bus)=>Some(bus),Err(error)=>{let _=tx.send(Err(error));return;}}}else{None};
   let capture_name=match CString::new(bus.as_ref().map_or_else(||text(&selected.name),|bus|bus.name().to_owned())){Ok(name)=>name,Err(_)=>{let _=tx.send(Err("invalid capture bus name".into()));return;}};
   // SAFETY: callback storage stays boxed until native is disconnected and freed on this worker.
   let ptr=unsafe{oink_pa_stream(capture_name.as_ptr(),1,rate,240,captured,(&mut *callback as *mut CaptureCallback).cast(),native_error.as_mut_ptr(),native_error.len())};
   if ptr.is_null(){let _=tx.send(Err(text(&native_error)));return;}
   let native=Native(ptr);let mut router=None;
   let setup=(||->Result<(),String>{if let Some(bus)=bus.take(){router=Some(routes::Router::start(bus,||{if unsafe{oink_pa_iterate(native.0)}<0{Err("PulseAudio capture failed during route setup".into())}else{Ok(())}})?);muted2.store(true,Ordering::Release);}
    // Observe original endpoint controls, even while PCM comes from the private bus.
    if unsafe{oink_pa_watch_output(native.0,name.as_ptr())}<0{return Err("cannot observe PulseAudio output changes".into());}Ok(())})();
   if let Err(e)=setup{let _=tx.send(Err(e));}else{
    // SAFETY: native stream is ready and remains valid throughout these queries.
    let frames=unsafe{oink_pa_buffer(native.0)};
    let info=CaptureInfo{backend:if mute{"pulseaudio_private_capture_bus".into()}else{"pulseaudio_monitor".into()},sample_rate:rate,requested_buffer_frames:Some(240),actual_buffer_frames:frames,physical_output_id:Some(id.clone()),physical_output_sample_rate:Some(selected.rate),timestamp_source:"PulseAudio stream latency estimate; hardware position unavailable".into(),diagnostics:serde_json::json!({"server":"PulseAudio or pipewire-pulse","scope":scope,"channels":2,"default_device_changed":false,"native_position_known":false,"local_silence":if mute{"owned application routes on private bus"}else{"local playback preserved"},"default_devices_changed":false}),..Default::default()};
    let _=tx.send(Ok(info));
    while !stop2.load(Ordering::Acquire){
     // SAFETY: nonblocking event dispatch and callbacks run only on this worker.
     let rc=unsafe{oink_pa_iterate(native.0)};if rc<0||callback.panicked{*error2.lock().unwrap_or_else(|e|e.into_inner())=Some("PulseAudio capture stream failed or overflowed".into());break;}
     if rc==2{changed2.store(true,Ordering::Release);break;}
     if rc==1{controls2.store(true,Ordering::Release);break;}if let Some(error)=router.as_ref().and_then(routes::Router::error){*error2.lock().unwrap_or_else(|e|e.into_inner())=Some(error);break;}
     callback.encoder.tick(Instant::now());thread::sleep(Duration::from_millis(1));
    }
   }
   drop(native);
   if let Some(mut router)=router{if let Some(error)=router.finish(){*cleanup2.lock().unwrap_or_else(|e|e.into_inner())=Some(error);}}
   muted2.store(false,Ordering::Release);crate::device_notifications::notify();
  }).map_err(|e|e.to_string())?;
        let info = match rx.recv() {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                let _ = worker.join();
                return Err("PulseAudio capture worker stopped during startup".into());
            }
        };
        Ok(Self {
            stop,
            error,
            cleanup,
            changed,
            controls_changed,
            muted,
            worker: Some(worker),
            info,
        })
    }
    pub fn info(&self) -> CaptureInfo {
        self.info.clone()
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn source_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }
    pub fn default_output_changed(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }
    pub fn local_controls_changed(&self) -> bool {
        self.controls_changed.load(Ordering::Acquire)
    }
    pub fn restart_reason(&self) -> Option<String> {
        self.default_output_changed()
            .then(|| "default PulseAudio output changed".into())
    }
    pub fn finish(&mut self) -> Option<String> {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        self.cleanup.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
#[derive(Clone, Copy)]
pub(crate) struct NativeOutputTiming {
    pub period_frames: u32,
    pub native_gap_frames: Option<u64>,
    pub callback_at: Instant,
    pub playback_at: Option<Instant>,
    pub sample_rate: u32,
    pub channels: u16,
    pub padding_frames: u32,
    pub buffer_frames: u32,
}
#[derive(Clone, serde::Serialize)]
pub(crate) struct NativeOutputInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub actual_period_frames: u32,
    pub requested_period_frames: Option<u32>,
    pub backend: &'static str,
    pub stream_latency_ms: Option<f64>,
    pub timing_method: &'static str,
}
struct PlaybackCallback {
    render: Box<dyn crate::playback::NativeRender>,
    panicked: bool,
    buffer_frames: u32,
}
unsafe extern "C" fn rendered(user: *mut c_void, data: *mut f32, len: usize, latency: u64, known: c_int, _mode: c_int) {
    // SAFETY: C passes a live exclusive writable float buffer and this worker's boxed callback.
    let state = unsafe { &mut *user.cast::<PlaybackCallback>() };
    let output = unsafe { std::slice::from_raw_parts_mut(data, len) };
    let at = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.render.fill(
            output,
            NativeOutputTiming {
                period_frames: (len / 2) as u32,
                native_gap_frames: None,
                callback_at: at,
                playback_at: (known != 0)
                    .then(|| at.checked_add(Duration::from_micros(latency)))
                    .flatten(),
                sample_rate: 48_000,
                channels: 2,
                padding_frames: 0,
                buffer_frames: state.buffer_frames,
            },
        );
        state.render.complete(at.elapsed());
    }));
    if result.is_err() {
        output.fill(0.0);
        state.panicked = true;
    }
}
pub(crate) struct NativePlayback {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    info: NativeOutputInfo,
}
impl NativePlayback {
    pub fn start(
        id: &str,
        requested: Option<u32>,
        render: Box<dyn crate::playback::NativeRender>,
        mut on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<Self, String> {
        let name = CString::new(id).map_err(|_| "invalid output ID")?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-pulse-playback".into())
            .spawn(move || {
                let mut cb = Box::new(PlaybackCallback {
                    render,
                    panicked: false,
                    buffer_frames: 0,
                });
                let mut e = [0; 512];
                // SAFETY: boxed callback remains valid and on this worker until native teardown.
                let ptr = unsafe {
                    oink_pa_stream(
                        name.as_ptr(),
                        0,
                        48_000,
                        requested.unwrap_or(0),
                        rendered,
                        (&mut *cb as *mut PlaybackCallback).cast(),
                        e.as_mut_ptr(),
                        e.len(),
                    )
                };
                if ptr.is_null() {
                    let _ = tx.send(Err(text(&e)));
                    return;
                }
                let native = Native(ptr);
                // SAFETY: native stream is ready.
                cb.buffer_frames = unsafe { oink_pa_buffer(native.0) };
                let _ = tx.send(Ok(cb.buffer_frames));
                while !s.load(Ordering::Acquire) {
                    if unsafe { oink_pa_playback_iterate(native.0) } < 0 || cb.panicked {
                        on_error("PulseAudio playback disconnected or failed".into());
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let frames = match rx.recv() {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                let _ = worker.join();
                return Err("PulseAudio playback worker stopped".into());
            }
        };
        Ok(Self {
            stop,
            worker: Some(worker),
            info: NativeOutputInfo {
                sample_rate: 48_000,
                channels: 2,
                actual_period_frames: frames,
                requested_period_frames: requested,
                backend: "pulseaudio_native",
                stream_latency_ms: None,
                timing_method: "PulseAudio latency estimate; hardware gap counters unavailable",
            },
        })
    }
    pub fn info(&self) -> NativeOutputInfo {
        self.info.clone()
    }
    pub fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.worker.take().is_none_or(|w| w.join().is_ok())
    }
}
impl Drop for NativePlayback {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub(crate) fn open_volume(id: &str) -> Result<Box<dyn crate::volume::Endpoint>, crate::volume::Fault> {
    let _ = device(Some(id)).map_err(|_| crate::volume::Fault::unavailable())?;
    Ok(Box::new(Volume {
        id: id.into(),
        generation: std::sync::atomic::AtomicU64::new(0),
    }))
}
struct Volume {
    id: String,
    generation: std::sync::atomic::AtomicU64,
}
impl crate::volume::Endpoint for Volume {
    fn changed(&self) -> bool {
        let generation = crate::device_notifications::generation();
        self.generation.swap(generation, Ordering::AcqRel) != generation
    }
    fn read(&mut self) -> Result<crate::volume::Reading, crate::volume::Fault> {
        let d = device(Some(&self.id)).map_err(|_| crate::volume::Fault::unavailable())?;
        Ok(crate::volume::Reading {
            percent: ((d.volume as u64 * 100 / 65_536).min(100)) as u8,
            muted: d.muted != 0,
        })
    }
    fn write(&mut self, r: crate::volume::Reading) -> Result<(), crate::volume::Fault> {
        let name = CString::new(raw_id(&self.id).map_err(|_| crate::volume::Fault::unavailable())?)
            .map_err(|_| crate::volume::Fault::unavailable())?;
        let mut e = [0; 512];
        if unsafe { oink_pa_volume(name.as_ptr(), r.percent as u32 * 65_536 / 100, e.as_mut_ptr(), e.len()) } < 0 {
            return Err(crate::volume::Fault::unavailable());
        }
        set_mute(&self.id, r.muted).map_err(|_| crate::volume::Fault::unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Tone(f32);
    impl crate::playback::NativeRender for Tone {
        fn fill(&mut self, out: &mut [f32], _: NativeOutputTiming) {
            for pair in out.chunks_mut(2) {
                let value = self.0.sin() * 0.15;
                self.0 = (self.0 + std::f32::consts::TAU * 440.0 / 48_000.0) % std::f32::consts::TAU;
                pair.fill(value);
            }
        }
        fn complete(&mut self, _: Duration) {}
    }
    #[test]
    fn exact_ids_reject_empty_foreign_and_nul_names() {
        assert!(raw_id("pulse:").is_err());
        assert!(raw_id("alsa:default").is_err());
        assert!(raw_id("pulse:bad\0name").is_err());
        assert_eq!(raw_id("pulse:exact name").unwrap(), "exact name");
    }
    struct ExternalTone {
        child: std::process::Child,
        writer: Option<JoinHandle<()>>,
    }
    impl ExternalTone {
        fn start() -> Self {
            use std::io::Write;
            let mut child = std::process::Command::new("pacat")
                .args([
                    "--playback",
                    "--raw",
                    "--device=oink_test_output",
                    "--client-name=oink-test-external",
                    "--latency-msec=20",
                    "--process-time-msec=5",
                    "--format=float32le",
                    "--rate=48000",
                    "--channels=2",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut pipe = child.stdin.take().unwrap();
            let writer = thread::spawn(move || {
                let mut phase = 0f32;
                let mut bytes = [0u8; 1920];
                loop {
                    for frame in bytes.chunks_exact_mut(8) {
                        let value = (phase.sin() * 0.15).to_le_bytes();
                        phase = (phase + std::f32::consts::TAU * 440.0 / 48_000.0) % std::f32::consts::TAU;
                        frame[..4].copy_from_slice(&value);
                        frame[4..].copy_from_slice(&value);
                    }
                    if pipe.write_all(&bytes).is_err() {
                        break;
                    }
                }
            });
            Self {
                child,
                writer: Some(writer),
            }
        }
    }
    impl Drop for ExternalTone {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(w) = self.writer.take() {
                let _ = w.join();
            }
        }
    }
    #[test]
    #[ignore = "requires isolated PulseAudio with oink_test_output and OINK_TEST_PULSE=1"]
    fn native_full_sink_starts_empty_and_resumes_without_silent_or_sparse_output() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        assert_eq!(std::env::var("OINK_TEST_PULSE").as_deref(), Ok("1"));
        assert!(std::env::var("PULSE_SERVER").is_ok_and(|s| s.starts_with("unix:")));
        assert_eq!(
            text(&device(Some("pulse:oink_test_output")).unwrap().name),
            "oink_test_output"
        );
        struct Monitor {
            child: std::process::Child,
            worker: Option<JoinHandle<Vec<(f64, f64)>>>,
        }
        impl Monitor {
            fn stop(&mut self) -> Vec<(f64, f64)> {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.worker.take().map(|w| w.join().unwrap()).unwrap_or_default()
            }
        }
        impl Drop for Monitor {
            fn drop(&mut self) {
                self.stop();
            }
        }
        let origin = Instant::now();
        let mut child = Command::new("parec")
            .args([
                "--device=oink_test_output.monitor",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "--process-time-msec=5",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut pipe = child.stdout.take().unwrap();
        let worker = thread::spawn(move || {
            let mut windows = Vec::new();
            let mut bytes = [0u8; 4800 * 8];
            while pipe.read_exact(&mut bytes).is_ok() {
                let energy: f64 = bytes
                    .chunks_exact(8)
                    .map(|frame| f64::from(f32::from_le_bytes(frame[..4].try_into().unwrap())).powi(2))
                    .sum();
                windows.push((origin.elapsed().as_secs_f64(), (energy / 4800.0).sqrt()));
            }
            windows
        });
        let mut monitor = Monitor {
            child,
            worker: Some(worker),
        };
        thread::sleep(Duration::from_millis(100));
        let mut output = crate::sink::Sink::start("pulse:oink_test_output").unwrap();
        // The native writer begins without a source. A zero prebuffer used to
        // leave subsequent relative writes permanently behind the read index.
        thread::sleep(Duration::from_millis(150));
        // A newly started isolated null sink can take an initial timer cycle to
        // run. Wait for real native demand before the bounded packet producer.
        let ready = Instant::now();
        let mut callbacks = 0;
        while callbacks < 3 && ready.elapsed() < Duration::from_secs(2) {
            callbacks += output.take_metrics().callbacks;
            thread::sleep(Duration::from_millis(5));
        }
        assert!(callbacks >= 3, "native playback did not start");
        let config = crate::dsp::Config::music_opus();
        let mut ranges = Vec::new();
        for burst in 0..2 {
            let mut source = output.source(config).unwrap();
            let mut encoder = crate::dsp::Encoder::new_with_config(config).unwrap();
            let start = Instant::now();
            for sequence in 0..60u64 {
                let pcm: Vec<f32> = (0..config.frame_size())
                    .flat_map(|frame| {
                        let sample =
                            ((sequence as usize * config.frame_size() + frame) as f64 * std::f64::consts::TAU * 997.0
                                / 48000.0)
                                .sin() as f32
                                * 0.05;
                        [sample, sample]
                    })
                    .collect();
                let payload = encoder.encode(&pcm).unwrap();
                assert!(
                    source.push(sequence, &payload, Instant::now(), Some(sequence * 20_000_000), None),
                    "packet {sequence} burst {burst}: stats={:?} metrics={:?} error={:?}",
                    source.stats(),
                    output.take_metrics(),
                    output.error()
                );
                source.poll();
                let deadline = start + Duration::from_millis((sequence + 1) * 20);
                if let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    thread::sleep(left);
                }
            }
            source.poll();
            assert_eq!(source.stats().decode_errors, 0);
            ranges.push((
                start.duration_since(origin).as_secs_f64() + 0.3,
                origin.elapsed().as_secs_f64() - 0.1,
            ));
            drop(source);
            if burst == 0 {
                thread::sleep(Duration::from_millis(250));
            }
        }
        thread::sleep(Duration::from_millis(150));
        assert!(output.error().is_none(), "{:?}", output.error());
        drop(output);
        let windows = monitor.stop();
        for (start, end) in ranges {
            let stable: Vec<_> = windows.iter().filter(|(at, _)| *at >= start && *at <= end).collect();
            assert!(stable.len() >= 6, "insufficient monitor windows: {windows:?}");
            assert!(
                stable.iter().all(|(_, rms)| (0.025..0.045).contains(rms)),
                "silent/sparse native playback: {windows:?}"
            );
        }
    }
    #[test]
    #[ignore = "requires isolated PulseAudio with default oink_test_output and OINK_TEST_PULSE=1"]
    fn native_monitor_keeps_pcm_while_muted_and_restores_only_its_lease() {
        assert_eq!(std::env::var("OINK_TEST_PULSE").as_deref(), Ok("1"));
        assert!(std::env::var("PULSE_SERVER").is_ok_and(|s| s.starts_with("unix:")));
        let before = endpoint_state(None).unwrap();
        assert_eq!(before.endpoint, "pulse:oink_test_output", "never touch a real output");
        assert!(!before.muted);
        // Verify native selected-output rendering separately. Its fixed-endpoint Oink stream is never rerouted.
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors2 = errors.clone();
        let mut playback = NativePlayback::start(
            raw_id(&before.endpoint).unwrap(),
            Some(240),
            Box::new(Tone(0.0)),
            Box::new(move |e| errors2.lock().unwrap().push(e)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(playback.shutdown());
        assert!(errors.lock().unwrap().is_empty());
        let _application = ExternalTone::start();
        thread::sleep(Duration::from_millis(100));
        let directory = tempfile::tempdir().unwrap();
        let (encoder, mut encoded) = source::channel(crate::dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let mut capture = Capture::start_encoded(encoder, true, directory.path().into()).unwrap();
        assert!(capture.source_muted());
        assert_eq!(
            endpoint_state(None).unwrap(),
            before,
            "muted capture must not modify endpoint controls or defaults"
        );
        let monitor_dir = tempfile::tempdir().unwrap();
        let (encoder, mut physical) = source::channel(crate::dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let mut monitor = Capture::start_encoded(encoder, false, monitor_dir.path().into()).unwrap();
        let ready = Instant::now();
        while (!encoded.has_signal() || encoded.frames() == 0) && ready.elapsed() < Duration::from_secs(5) {
            while encoded.try_recv().is_some() {}
            while physical.try_recv().is_some() {}
            thread::sleep(Duration::from_millis(5));
        }
        while encoded.try_recv().is_some() {}
        while physical.try_recv().is_some() {}
        let start = Instant::now();
        let (mut non_silent, mut audible) = (0, 0);
        while start.elapsed() < Duration::from_secs(1) {
            while let Some(packet) = encoded.try_recv() {
                if start.elapsed() > Duration::from_millis(200) && !packet.silent {
                    non_silent += 1;
                }
            }
            while let Some(packet) = physical.try_recv() {
                if start.elapsed() > Duration::from_millis(200) && !packet.silent {
                    audible += 1;
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            non_silent > 10,
            "muted capture must preserve real nonzero application PCM: frames={}, signal={}, error={:?}, muted={}, physical_frames={}",
            encoded.frames(),
            encoded.has_signal(),
            capture.error(),
            capture.source_muted(),
            physical.frames()
        );
        assert_eq!(
            audible, 0,
            "the original physical-output monitor must be silent while forwarding-only"
        );
        assert!(capture.error().is_none(), "{:?}", capture.error());
        assert!(monitor.finish().is_none());
        let finish = capture.finish();
        assert!(finish.is_none(), "{finish:?}");
        assert_eq!(endpoint_state(None).unwrap(), before);
        assert!(
            !modules().unwrap().iter().any(|m| m["argument"]
                .as_str()
                .is_some_and(|a| a.contains("sink_name=oink_capture_"))),
            "private capture module must actually be unloaded"
        );
        // A user moving the application away relinquishes that route; cleanup never puts it back on our bus.
        let (encoder, _encoded) = source::channel(crate::dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let mut capture = Capture::start_encoded(encoder, true, directory.path().into()).unwrap();
        let streams: Vec<serde_json::Value> =
            serde_json::from_str(&crate::microphone::linux::pactl(&["--format=json", "list", "sink-inputs"]).unwrap())
                .unwrap();
        let application = streams
            .iter()
            .find(|s| s["properties"]["application.name"].as_str() == Some("oink-test-external"))
            .unwrap();
        let index_value = application["index"].as_u64().unwrap();
        let index = index_value.to_string();
        crate::microphone::linux::pactl(&["move-sink-input", &index, "oink_test_output"]).unwrap();
        thread::sleep(Duration::from_millis(350));
        let finish = capture.finish();
        assert!(finish.is_none(), "{finish:?}");
        assert_eq!(endpoint_state(None).unwrap(), before);
        let streams: Vec<serde_json::Value> =
            serde_json::from_str(&crate::microphone::linux::pactl(&["--format=json", "list", "sink-inputs"]).unwrap())
                .unwrap();
        let expected = device(None).unwrap().index as u64;
        assert_eq!(
            streams
                .iter()
                .find(|s| s["index"].as_u64() == Some(index_value))
                .unwrap()["sink"]
                .as_u64(),
            Some(expected)
        );
        assert!(!modules().unwrap().iter().any(|m| {
            m["argument"]
                .as_str()
                .is_some_and(|a| a.contains("sink_name=oink_capture_"))
        }));
    }
}

static SOURCE_OUTPUT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub(crate) fn consumer_generation() -> u64 {
    start_inventory_notifications();
    SOURCE_OUTPUT_GENERATION.load(Ordering::Acquire)
}
pub(crate) fn start_inventory_notifications() {
    static START: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    START.get_or_init(|| {
        let _ = thread::Builder::new().name("oink-pulse-notify".into()).spawn(|| {
            loop {
                unsafe extern "C" {
                    fn oink_pa_inventory_open(error: *mut c_char, size: usize) -> *mut c_void;
                    fn oink_pa_take_changed(p: *mut c_void) -> c_int;
                }
                let mut error = [0; 512];
                // SAFETY: the returned context is owned and dispatched only by this notification worker.
                let pointer = unsafe { oink_pa_inventory_open(error.as_mut_ptr(), error.len()) };
                if pointer.is_null() {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
                let native = Native(pointer);
                SOURCE_OUTPUT_GENERATION.fetch_add(1, Ordering::AcqRel);
                crate::device_notifications::invalidate();
                loop {
                    if unsafe { oink_pa_iterate(native.0) } < 0 {
                        SOURCE_OUTPUT_GENERATION.fetch_add(1, Ordering::AcqRel);
                        crate::device_notifications::invalidate();
                        break;
                    }
                    let mask = unsafe { oink_pa_take_changed(native.0) };
                    if mask & 1 != 0 {
                        crate::device_notifications::invalidate();
                    }
                    if mask & 2 != 0 {
                        SOURCE_OUTPUT_GENERATION.fetch_add(1, Ordering::AcqRel);
                        crate::device_notifications::notify();
                    }
                    if mask & 4 != 0 {
                        crate::device_notifications::notify();
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    });
}
