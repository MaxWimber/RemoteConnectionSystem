//! The capture and encode thread: Desktop Duplication -> encoder -> `FrameSink`.
//!
//! It follows `rcs-bench capture`: the stream-rate credit (D-019), repeats of the last frame on a static screen,
//! a new duplication after `ACCESS_LOST` and a new device, capture and encoder after the GPU was lost. Frames are
//! encoded only while a browser is connected, and the whole pipeline is closed while nobody is, so the card's one
//! encoder is free for other programs (D-028). No network or disk I/O happens here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_capture::{
    Capture, CaptureError, CaptureEvent, DxgiCapture, Pointer, PointerShape, TimerResolution, dxgi,
};
use rcs_encode::amf::EncoderConfig;
use rcs_encode::{Backend, EncodeError, EncodedFrame, Encoder, SubmitStats};
use rcs_webrtc::{FrameSink, StreamMode, VideoFrame};
use tracing::{info, warn};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

use crate::recovery::{AmfFailures, Backoff};

const AMD_VENDOR_ID: u32 = 0x1002;
/// Credit a new frame needs, see D-019.
const PACING_SHARE: f64 = 0.9;
/// Minimum frame rate on a static screen.
const REPEAT_FPS: f64 = 15.0;
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the thread waits for frames while nobody is connected.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// Nobody connected for this long closes the pipeline (D-028).
const IDLE_RELEASE: Duration = Duration::from_secs(10);
/// How often a closed pipeline looks for a browser; opening it then takes about 0.1 s.
const IDLE_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct Config {
    pub display: u32,
    pub backend: Backend,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate: i64,
    /// Testing only: every AMF encoder fails after this many frames (D-028).
    pub simulate_amf_failure: Option<u64>,
}

/// What the pipeline tells the status request.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub encoder: String,
    pub adapter: String,
    pub input: (u32, u32),
    /// The captured monitor in desktop pixels: left, top, right, bottom. The input thread maps positions onto it.
    pub monitor: Option<[i32; 4]>,
    pub last_error: Option<String>,
    /// Whether the pipeline, and with it the encoder, is open now.
    pub open: bool,
}

#[derive(Default)]
pub struct Stats {
    pub frames_encoded: AtomicU64,
    pub restarts: AtomicU64,
    pub info: Mutex<Info>,
}

pub fn spawn(
    config: Config,
    sink: FrameSink,
    shapes: SyncSender<PointerShape>,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("capture-encode".into())
        .spawn(move || run(&config, &sink, &shapes, &stats, &stop))
}

/// The pointer's hot spot as 0..65535 over an output of `size`, and whether it is on that output.
pub fn hot_spot(pointer: &Pointer, size: (u32, u32)) -> (bool, u16, u16) {
    let x = i64::from(pointer.x) + i64::from(pointer.shape.hot_x);
    let y = i64::from(pointer.y) + i64::from(pointer.shape.hot_y);
    let (w, h) = (i64::from(size.0.max(1)), i64::from(size.1.max(1)));
    let inside = (0..w).contains(&x) && (0..h).contains(&y);
    let along = |v: i64, n: i64| {
        if n <= 1 {
            0
        } else {
            ((v.clamp(0, n - 1) * 65_535 + (n - 1) / 2) / (n - 1)) as u16
        }
    };
    (pointer.visible && inside, along(x, w), along(y, h))
}

/// Passes pointer changes on: the position straight to the network thread, a new shape to the input thread.
#[derive(Default)]
struct PointerSeen {
    moves: u64,
    shapes: u64,
}

impl PointerSeen {
    fn forward(
        &mut self,
        capture: &DxgiCapture,
        sink: &FrameSink,
        shapes: &SyncSender<PointerShape>,
    ) {
        let p = capture.pointer();
        if p.moves == self.moves && p.shapes == self.shapes {
            return;
        }
        let (visible, x, y) = hot_spot(p, capture.size());
        sink.set_cursor(visible, x, y);
        if p.shapes != self.shapes && p.shape.width > 0 {
            // A full queue means the input thread is behind; the next shape replaces this one.
            let _ = shapes.try_send(p.shape.clone());
        }
        self.moves = p.moves;
        self.shapes = p.shapes;
    }
}

fn note_error(stats: &Stats, error: String) {
    warn!(%error, "pipeline");
    if let Ok(mut info) = stats.info.lock() {
        info.last_error = Some(error);
    }
}

/// Why a pipeline had to be built again.
#[derive(Debug)]
enum Failure {
    /// Encoding failed on a working device; `amf` says whether the encoder was AMF.
    Encoder { amf: bool, error: String },
    /// Anything else: opening, capture, a lost device.
    Other(String),
}

impl Failure {
    fn text(&self) -> &str {
        match self {
            Self::Encoder { error, .. } | Self::Other(error) => error,
        }
    }
}

fn set_open(stats: &Stats, open: bool) {
    if let Ok(mut info) = stats.info.lock() {
        info.open = open;
    }
}

/// Sleeps for `pause`, waking early when the agent stops.
fn pause_unless_stopped(pause: Duration, stop: &AtomicBool) {
    let until = Instant::now() + pause;
    while !stop.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(IDLE_POLL));
    }
}

fn run(
    config: &Config,
    sink: &FrameSink,
    shapes: &SyncSender<PointerShape>,
    stats: &Stats,
    stop: &AtomicBool,
) {
    let _timer = TimerResolution::new(1);
    let mut backoff = Backoff::default();
    let mut amf_failures = AmfFailures::default();
    // `auto` after repeated AMF failures, until the browser disconnects (D-028).
    let mut use_mf = false;
    // Opened once at the start so the input thread learns the monitor, then closed when nobody connects.
    let mut open_now = true;
    while !stop.load(Ordering::Relaxed) {
        if !open_now && !sink.connected() {
            if use_mf {
                info!("browser gone, the next connection starts with AMF again");
                use_mf = false;
            }
            amf_failures.clear();
            backoff.reset();
            std::thread::sleep(IDLE_POLL);
            continue;
        }
        open_now = false;
        let backend = if use_mf { Backend::Mf } else { config.backend };
        let opened = Instant::now();
        let failure = match Pipeline::open(config, backend, sink.stream_mode(), stats) {
            Ok(mut pipeline) => {
                set_open(stats, true);
                let result = pipeline.run(config, backend, sink, shapes, stats, stop);
                drop(pipeline);
                set_open(stats, false);
                match result {
                    Ok(()) => continue,
                    Err(failure) => failure,
                }
            }
            Err(error) => Failure::Other(error),
        };
        stats.restarts.fetch_add(1, Ordering::Relaxed);
        note_error(stats, failure.text().to_owned());
        if matches!(failure, Failure::Encoder { amf: true, .. })
            && config.backend == Backend::Auto
            && amf_failures.record(Instant::now())
        {
            warn!("AMF keeps failing, Media Foundation until the browser disconnects");
            use_mf = true;
        }
        pause_unless_stopped(backoff.after_failure(opened.elapsed()), stop);
    }
    info!("pipeline stopped");
}

/// The encoder for `mode`: its picture size, scaled from the captured `input` by the encoder itself (D-030).
fn encoder_config(config: &Config, mode: StreamMode, input: (u32, u32)) -> EncoderConfig {
    let (width, height) = mode.size((config.width, config.height));
    let mut c = EncoderConfig::low_latency(width, height, config.fps, config.bitrate);
    c.input_width = input.0 as i32;
    c.input_height = input.1 as i32;
    c
}

fn open_encoder(
    config: &Config,
    backend: Backend,
    device: &ID3D11Device,
    mode: StreamMode,
    input: (u32, u32),
) -> Result<Box<dyn Encoder>, EncodeError> {
    let encoder = rcs_encode::open(backend, device, &encoder_config(config, mode, input))?;
    Ok(match config.simulate_amf_failure {
        Some(frames) if encoder.backend() == "amf" => Box::new(FailingEncoder {
            inner: encoder,
            frames_left: frames,
        }),
        _ => encoder,
    })
}

/// Stands in while one encoder is closed and the next opens: the card may not take two at once.
struct NoEncoder;

impl Encoder for NoEncoder {
    fn encode(
        &mut self,
        _frame: &rcs_capture::CapturedFrame<'_>,
        _pts: i64,
        _force_idr: bool,
    ) -> Result<SubmitStats, EncodeError> {
        Err(EncodeError("no encoder".into()))
    }

    fn poll(&mut self, _out: &mut EncodedFrame) -> Result<bool, EncodeError> {
        Ok(false)
    }

    fn set_bitrate(&mut self, _bits_per_second: u32) -> Result<(), EncodeError> {
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "none"
    }
}

/// Testing only: an encoder that fails every frame after `frames_left` frames (D-028).
struct FailingEncoder {
    inner: Box<dyn Encoder>,
    frames_left: u64,
}

impl Encoder for FailingEncoder {
    fn encode(
        &mut self,
        frame: &rcs_capture::CapturedFrame<'_>,
        pts: i64,
        force_idr: bool,
    ) -> Result<SubmitStats, EncodeError> {
        if self.frames_left == 0 {
            return Err(EncodeError("simulated AMF failure".into()));
        }
        self.frames_left -= 1;
        self.inner.encode(frame, pts, force_idr)
    }

    fn poll(&mut self, out: &mut EncodedFrame) -> Result<bool, EncodeError> {
        self.inner.poll(out)
    }

    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
        self.inner.set_bitrate(bits_per_second)
    }

    fn backend(&self) -> &'static str {
        self.inner.backend()
    }
}

fn monitor_of(adapter: &dxgi::AdapterInfo, display: u32) -> Option<[i32; 4]> {
    adapter
        .outputs
        .iter()
        .find(|o| o.index == display)
        .map(|o| o.desktop)
}

struct Pipeline {
    adapter_luid: u64,
    device: ID3D11Device,
    capture: DxgiCapture,
    encoder: Box<dyn Encoder>,
    input: (u32, u32),
    mode: StreamMode,
}

impl Pipeline {
    fn open(
        config: &Config,
        backend: Backend,
        mode: StreamMode,
        stats: &Stats,
    ) -> Result<Self, String> {
        let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
        let adapter = adapters
            .iter()
            .find(|a| a.info.vendor_id == AMD_VENDOR_ID && !a.info.software)
            .or_else(|| adapters.iter().find(|a| !a.info.software))
            .ok_or("no hardware adapter")?;
        let (device, _) = dxgi::create_device(&adapter.adapter).map_err(|e| dxgi::describe(&e))?;
        let capture = DxgiCapture::new(adapter.adapter.clone(), device.clone(), config.display)
            .map_err(|e| e.to_string())?;
        let input = capture.size();
        let encoder = open_encoder(config, backend, &device, mode, input)
            .map_err(|e| format!("encoder {}: {e}", backend.name()))?;
        info!(
            adapter = %adapter.info.name,
            encoder = encoder.backend(),
            width = input.0,
            height = input.1,
            mode = mode.name(),
            "pipeline opened"
        );
        if let Ok(mut info) = stats.info.lock() {
            info.encoder = encoder.backend().into();
            info.adapter = adapter.info.name.clone();
            info.input = input;
            info.monitor = monitor_of(&adapter.info, config.display);
        }
        Ok(Self {
            adapter_luid: adapter.info.luid,
            device,
            capture,
            encoder,
            input,
            mode,
        })
    }

    /// A new encoder for `mode` or the new input size; the old one is closed first.
    fn reopen_encoder(
        &mut self,
        config: &Config,
        backend: Backend,
        mode: StreamMode,
    ) -> Result<(), EncodeError> {
        self.encoder = Box::new(NoEncoder);
        self.encoder = open_encoder(config, backend, &self.device, mode, self.input)?;
        self.mode = mode;
        Ok(())
    }

    /// Runs until `stop` or until nobody was connected for `IDLE_RELEASE`, or returns why the pipeline must be
    /// built again.
    fn run(
        &mut self,
        config: &Config,
        backend: Backend,
        sink: &FrameSink,
        shapes: &SyncSender<PointerShape>,
        stats: &Stats,
        stop: &AtomicBool,
    ) -> Result<(), Failure> {
        let mut pointer = PointerSeen::default();
        let fps = f64::from(config.fps.max(1));
        let frame_interval = Duration::from_secs_f64(1.0 / fps);
        let repeat_interval = Duration::from_secs_f64(1.0 / REPEAT_FPS);
        let started = Instant::now();
        let mut packet = EncodedFrame::default();
        let mut last_pts = -1i64;
        let mut last_submit = started;
        let mut credit = 1.0f64;
        let mut credit_at = started;
        let mut was_connected = false;
        let mut force_idr = true;
        // The bitrate the encoder was given; the rate control may want another (D-027).
        let mut bitrate = config.bitrate.clamp(1, i64::from(u32::MAX)) as u32;
        let mut idle_since = None;

        while !stop.load(Ordering::Relaxed) {
            let connected = sink.connected();
            if connected && !was_connected {
                force_idr = true;
            }
            was_connected = connected;
            if connected {
                idle_since = None;
            } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_RELEASE {
                info!("nobody connected, pipeline and encoder closed");
                return Ok(());
            }
            if sink.take_keyframe_request() {
                force_idr = true;
            }
            // Another stream mode: another picture size from the same capture, starting with an IDR (D-030).
            let wanted = sink.stream_mode();
            if wanted != self.mode {
                let opened = Instant::now();
                self.reopen_encoder(config, backend, wanted).map_err(|e| {
                    Failure::Other(format!("encoder for mode {}: {e}", wanted.name()))
                })?;
                let (width, height) = wanted.size((config.width, config.height));
                info!(
                    mode = wanted.name(),
                    width,
                    height,
                    ms = opened.elapsed().as_secs_f64() * 1000.0,
                    "stream mode, new encoder"
                );
                force_idr = true;
                // The new encoder starts at the configured bitrate; the target follows right below.
                bitrate = config.bitrate.clamp(1, i64::from(u32::MAX)) as u32;
            }
            let target = sink.target_bitrate();
            if connected && target > 0 && target != bitrate {
                match self.encoder.set_bitrate(target) {
                    Ok(()) => bitrate = target,
                    Err(e) => {
                        // Not tried again for this target; a broken encoder shows up in encode anyway.
                        warn!(error = %e, target, "set_bitrate");
                        bitrate = target;
                    }
                }
            }

            // Stream-rate credit (D-019).
            let now = Instant::now();
            credit = (credit + (now - credit_at).as_secs_f64() * fps).min(1.0);
            credit_at = now;
            if connected && credit < PACING_SHARE {
                std::thread::sleep(Duration::from_secs_f64((PACING_SHARE - credit) / fps));
                let now = Instant::now();
                credit = (credit + (now - credit_at).as_secs_f64() * fps).min(1.0);
                credit_at = now;
            }

            let wait = if !connected {
                IDLE_WAIT
            } else if self.capture.last_frame().is_some() {
                repeat_interval
                    .saturating_sub(last_submit.elapsed())
                    .clamp(Duration::from_millis(1), frame_interval)
            } else {
                frame_interval
            };
            let mut repeat_due = false;
            match self.capture.next_frame(wait) {
                Ok(CaptureEvent::Frame(frame)) => {
                    if connected {
                        credit -= 1.0;
                        last_submit = Instant::now();
                        let idr = std::mem::take(&mut force_idr);
                        encode(
                            self.encoder.as_mut(),
                            &frame,
                            started,
                            &mut last_pts,
                            idr,
                            &mut packet,
                            sink,
                            stats,
                        )
                        .map_err(|e| self.lost_or(e))?;
                    }
                }
                Ok(CaptureEvent::Timeout) => {
                    repeat_due = connected && last_submit.elapsed() >= repeat_interval;
                }
                Ok(CaptureEvent::Reinitialize) => {
                    force_idr = true;
                    info!(
                        desktop = ?rcs_capture::desktop::attached_name(),
                        "capture reinitialized"
                    );
                    let size = self.capture.size();
                    if size != self.input && size.0 > 0 {
                        info!(?size, "display size changed, new encoder");
                        self.input = size;
                        let mode = self.mode;
                        self.reopen_encoder(config, backend, mode).map_err(|e| {
                            Failure::Other(format!("encoder after a mode change: {e}"))
                        })?;
                        // The new encoder starts at the configured bitrate; the next frame applies the target.
                        bitrate = config.bitrate.clamp(1, i64::from(u32::MAX)) as u32;
                        // A mode change moves the monitor's corners; reading them again is rare and cheap.
                        let monitor = dxgi::enumerate_adapters().ok().and_then(|list| {
                            list.iter()
                                .find(|a| a.info.luid == self.adapter_luid)
                                .and_then(|a| monitor_of(&a.info, config.display))
                        });
                        if let Ok(mut info) = stats.info.lock() {
                            info.input = size;
                            if monitor.is_some() {
                                info.monitor = monitor;
                            }
                        }
                    }
                }
                Err(CaptureError::DeviceRemoved(e)) => {
                    return Err(Failure::Other(format!(
                        "device lost: {}",
                        dxgi::describe(&e)
                    )));
                }
                Err(e) => return Err(Failure::Other(e.to_string())),
            }
            pointer.forward(&self.capture, sink, shapes);
            if repeat_due && let Some(frame) = self.capture.last_frame() {
                last_submit = Instant::now();
                let idr = std::mem::take(&mut force_idr);
                encode(
                    self.encoder.as_mut(),
                    &frame,
                    started,
                    &mut last_pts,
                    idr,
                    &mut packet,
                    sink,
                    stats,
                )
                .map_err(|e| self.lost_or(e))?;
            }
        }
        Ok(())
    }

    /// An encoding error is the encoder's own only while the device still works.
    fn lost_or(&self, error: String) -> Failure {
        // SAFETY: the device is valid; the call only reads its state.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            Failure::Other(format!("device lost while encoding: {error}"))
        } else {
            Failure::Encoder {
                amf: self.encoder.backend() == "amf",
                error,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode(
    encoder: &mut dyn Encoder,
    frame: &rcs_capture::CapturedFrame<'_>,
    started: Instant,
    last_pts: &mut i64,
    force_idr: bool,
    packet: &mut EncodedFrame,
    sink: &FrameSink,
    stats: &Stats,
) -> Result<(), String> {
    // pts in 100 ns units, strictly growing also for repeats.
    let pts = ((frame.acquired - started).as_nanos() / 100) as i64;
    let pts = pts.max(*last_pts + 1);
    *last_pts = pts;
    let submitted = Instant::now();
    encoder
        .encode(frame, pts, force_idr)
        .map_err(|e| format!("encode: {e}"))?;
    loop {
        if encoder.poll(packet).map_err(|e| format!("poll: {e}"))? {
            break;
        }
        if submitted.elapsed() > OUTPUT_TIMEOUT {
            // The frame is lost; the next one should be an IDR so the browser recovers.
            return Err("the encoder gave no output".into());
        }
    }
    let mut data = sink.buffer();
    data.extend_from_slice(&packet.data);
    sink.push(VideoFrame {
        data,
        captured: frame.acquired,
        keyframe: packet.keyframe,
    });
    stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_spot_is_the_shape_corner_plus_the_hot_offset() {
        let mut p = Pointer {
            visible: true,
            x: 958,
            y: 530,
            ..Pointer::default()
        };
        p.shape.hot_x = 2;
        p.shape.hot_y = 10;
        assert_eq!(hot_spot(&p, (1920, 1080)), (true, 32785, 32798));
        p.x = 0;
        p.y = 0;
        p.shape.hot_x = 0;
        p.shape.hot_y = 0;
        assert_eq!(hot_spot(&p, (1920, 1080)), (true, 0, 0));
        p.x = 1919;
        p.y = 1079;
        assert_eq!(hot_spot(&p, (1920, 1080)), (true, 65535, 65535));
        // The corner may be off the output while the hot spot is on it, and the reverse.
        p.x = -5;
        p.shape.hot_x = 5;
        assert!(hot_spot(&p, (1920, 1080)).0);
        p.x = 1919;
        assert!(!hot_spot(&p, (1920, 1080)).0, "on the next monitor");
        p.x = 100;
        p.visible = false;
        assert!(!hot_spot(&p, (1920, 1080)).0);
    }
}
