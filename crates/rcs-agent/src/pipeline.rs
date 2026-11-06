//! The capture and encode thread: Desktop Duplication -> encoder -> `FrameSink`.
//!
//! It follows `rcs-bench capture`: the stream-rate credit (D-019), repeats of the last frame on a static screen,
//! a new duplication after `ACCESS_LOST` and a new device, capture and encoder after the GPU was lost. Frames are
//! encoded only while a browser is connected. No network or disk I/O happens here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_capture::{
    Capture, CaptureError, CaptureEvent, DxgiCapture, Pointer, PointerShape, TimerResolution, dxgi,
};
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
    /// The captured monitor in desktop pixels: left, top, right, bottom. The input thread maps positions onto it.
    pub monitor: Option<[i32; 4]>,
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

fn run(
    config: &Config,
    sink: &FrameSink,
    shapes: &SyncSender<PointerShape>,
    stats: &Stats,
    stop: &AtomicBool,
) {
    let _timer = TimerResolution::new(1);
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        if !first {
            stats.restarts.fetch_add(1, Ordering::Relaxed);
        }
        first = false;
        match Pipeline::open(config, stats) {
            Ok(mut pipeline) => {
                if let Err(e) = pipeline.run(config, sink, shapes, stats, stop) {
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
            info.monitor = monitor_of(&adapter.info, config.display);
        }
        Ok(Self {
            adapter_luid: adapter.info.luid,
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
        shapes: &SyncSender<PointerShape>,
        stats: &Stats,
        stop: &AtomicBool,
    ) -> Result<(), String> {
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
                    return Err(format!("device lost: {}", dxgi::describe(&e)));
                }
                Err(e) => return Err(e.to_string()),
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
