//! The capture and encode thread: Desktop Duplication -> encoder -> `FrameSink`.
//!
//! It follows `rcs-bench capture`: the stream-rate credit (D-019), repeats of the last frame on a static screen,
//! a new duplication after `ACCESS_LOST` and a new device, capture and encoder after the GPU was lost. Frames are
//! encoded only while a browser is connected. No network or disk I/O happens here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_capture::{Capture, CaptureError, CaptureEvent, DxgiCapture, TimerResolution, dxgi};
use rcs_encode::amf::EncoderConfig;
use rcs_encode::{Backend, EncodedFrame, Encoder};
use rcs_webrtc::{FrameSink, VideoFrame};
use tracing::{info, warn};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

const AMD_VENDOR_ID: u32 = 0x1002;
/// Credit a new frame needs, see D-019.
const PACING_SHARE: f64 = 0.9;
/// Minimum frame rate on a static screen.
const REPEAT_FPS: f64 = 15.0;
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the thread waits for frames while nobody is connected.
const IDLE_WAIT: Duration = Duration::from_millis(100);
const RETRY_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct Config {
    pub display: u32,
    pub backend: Backend,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate: i64,
}

/// What the pipeline tells the status request.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub encoder: String,
    pub adapter: String,
    pub input: (u32, u32),
    pub last_error: Option<String>,
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
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("capture-encode".into())
        .spawn(move || run(&config, &sink, &stats, &stop))
}

fn note_error(stats: &Stats, error: String) {
    warn!(%error, "pipeline");
    if let Ok(mut info) = stats.info.lock() {
        info.last_error = Some(error);
    }
}

fn run(config: &Config, sink: &FrameSink, stats: &Stats, stop: &AtomicBool) {
    let _timer = TimerResolution::new(1);
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !first {
            stats.restarts.fetch_add(1, Ordering::Relaxed);
        }
        first = false;
        match Pipeline::open(config, stats) {
            Ok(mut pipeline) => {
                if let Err(e) = pipeline.run(config, sink, stats, stop) {
                    note_error(stats, e);
                    pipeline.capture.release();
                }
            }
            Err(e) => {
                note_error(stats, e);
                std::thread::sleep(RETRY_AFTER);
            }
        }
    }
    info!("pipeline stopped");
}

fn encoder_config(config: &Config, input: (u32, u32)) -> EncoderConfig {
    let mut c = EncoderConfig::low_latency(config.width, config.height, config.fps, config.bitrate);
    c.input_width = input.0 as i32;
    c.input_height = input.1 as i32;
    c
}

struct Pipeline {
    device: ID3D11Device,
    capture: DxgiCapture,
    encoder: Box<dyn Encoder>,
    input: (u32, u32),
}

impl Pipeline {
    fn open(config: &Config, stats: &Stats) -> Result<Self, String> {
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
        let encoder = rcs_encode::open(config.backend, &device, &encoder_config(config, input))
            .map_err(|e| format!("encoder {}: {e}", config.backend.name()))?;
        info!(
            adapter = %adapter.info.name,
            encoder = encoder.backend(),
            width = input.0,
            height = input.1,
            "pipeline opened"
        );
        if let Ok(mut info) = stats.info.lock() {
            info.encoder = encoder.backend().into();
            info.adapter = adapter.info.name.clone();
            info.input = input;
        }
        Ok(Self {
            device,
            capture,
            encoder,
            input,
        })
    }

    /// Runs until `stop`, or returns why the pipeline must be built again.
    fn run(
        &mut self,
        config: &Config,
        sink: &FrameSink,
        stats: &Stats,
        stop: &AtomicBool,
    ) -> Result<(), String> {
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

        while !stop.load(Ordering::Relaxed) {
            let connected = sink.connected();
            if connected && !was_connected {
                force_idr = true;
            }
            was_connected = connected;
            if sink.take_keyframe_request() {
                force_idr = true;
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
                    let size = self.capture.size();
                    if size != self.input && size.0 > 0 {
                        info!(?size, "display size changed, new encoder");
                        self.encoder = rcs_encode::open(
                            config.backend,
                            &self.device,
                            &encoder_config(config, size),
                        )
                        .map_err(|e| format!("encoder after a mode change: {e}"))?;
                        self.input = size;
                        if let Ok(mut info) = stats.info.lock() {
                            info.input = size;
                        }
                    }
                }
                Err(CaptureError::DeviceRemoved(e)) => {
                    return Err(format!("device lost: {}", dxgi::describe(&e)));
                }
                Err(e) => return Err(e.to_string()),
            }
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

    fn lost_or(&self, error: String) -> String {
        // SAFETY: the device is valid; the call only reads its state.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            format!("device lost while encoding: {error}")
        } else {
            error
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
