//! `rcs-bench capture`: Desktop Duplication -> AMF converter -> AMF encoder, written to a `.h264` file.
//!
//! The capture and encode loop does no disk I/O: packets go to a writer thread through a fixed set of buffers.
//! The report shows per-stage timings, histograms, skipped frames and the facts that the frame stays in video memory.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_capture::{
    Capture, CaptureError, CaptureEvent, CapturedFrame, DxgiCapture, TimerResolution, dxgi,
};
use rcs_encode::amf::EncoderConfig;
use rcs_encode::{EncodedFrame, Encoder, h264};
use rcs_protocol::BUILD_ID;
use serde_json::{Value, json};
use tracing::{info, info_span, warn};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;

use crate::logging;
use crate::options::Options;
use crate::probe;
use crate::stats::{histogram, summarize};

const REPORT_FILE: &str = "capture.json";
const STREAM_FILE: &str = "capture.h264";
const WRITER_BUFFERS: usize = 8;
const WRITER_BUFFER_BYTES: usize = 1 << 20;
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);
/// A frame that `AcquireNextFrame` returns faster than this was already waiting: the loop was busy when it came.
const PENDING_FRAME_MS: f64 = 1.0;
/// Credit a new frame needs, in frames. Below 1, so that a source at the display rate is never held back by timer
/// jitter; each frame still costs a whole credit, so the average stays at the stream rate.
const PACING_SHARE: f64 = 0.9;

/// Per-frame stage times in milliseconds.
#[derive(Default)]
struct Stages {
    capture_wait: Vec<f64>,
    present_to_acquire: Vec<f64>,
    copy: Vec<f64>,
    convert: Vec<f64>,
    submit: Vec<f64>,
    encode: Vec<f64>,
    total: Vec<f64>,
    frame_bytes: Vec<f64>,
}

impl Stages {
    fn with_capacity(n: usize) -> Self {
        let v = || Vec::with_capacity(n);
        Self {
            capture_wait: v(),
            present_to_acquire: v(),
            copy: v(),
            convert: v(),
            submit: v(),
            encode: v(),
            total: v(),
            frame_bytes: v(),
        }
    }

    fn report(&self) -> Value {
        let stage = |values: &[f64]| json!({ "summary": summarize(values), "histogram": histogram(values) });
        json!({
            "capture_wait_ms": stage(&self.capture_wait),
            "present_to_acquire_ms": stage(&self.present_to_acquire),
            "copy_ms": stage(&self.copy),
            "convert_ms": stage(&self.convert),
            "submit_ms": stage(&self.submit),
            "encode_ms": stage(&self.encode),
            "total_ms": stage(&self.total),
            "frame_bytes": summarize(&self.frame_bytes),
        })
    }
}

#[derive(Default)]
struct Counters {
    captured: u64,
    repeated: u64,
    reinitialized: u64,
    encoded: u64,
    idr: u64,
    forced_idr: u64,
    bytes: u64,
    largest: usize,
    lost_outputs: u64,
    pts_mismatch: u64,
    input_full: u64,
    input_in_video_memory: u64,
    input_elsewhere: u64,
    converted_in_video_memory: u64,
    converted_elsewhere: u64,
    late_frames: u64,
    writer_stalls: u64,
    encoders_created: u64,
    /// Desktop presents merged into one captured frame while the loop was busy: frames the pipeline missed.
    merged_while_busy: u64,
    /// Presents merged although the loop was waiting: the source presented twice within one composition.
    merged_while_waiting: u64,
    /// Presents merged while the loop held back to the stream rate: a source faster than the stream.
    merged_by_pacing: u64,
    /// The first frames with any merge: captured frame number, merged presents, wait in milliseconds.
    merges: Vec<(u64, u32, f64)>,
    /// Rebuilds of device, capture and encoder after the GPU was lost, and how long each took.
    device_rebuilds: u64,
    rebuild_ms: Vec<f64>,
}

const MERGES_KEPT: usize = 64;

/// Packets go to disk on this thread; the loop takes empty buffers back from it.
struct Writer {
    full: Option<SyncSender<Vec<u8>>>,
    empty: Receiver<Vec<u8>>,
    thread: Option<JoinHandle<std::io::Result<u64>>>,
}

impl Writer {
    fn start(path: &Path) -> Result<Self, String> {
        let file =
            File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let (full_tx, full_rx) = sync_channel::<Vec<u8>>(WRITER_BUFFERS);
        let (empty_tx, empty_rx) = sync_channel::<Vec<u8>>(WRITER_BUFFERS);
        for _ in 0..WRITER_BUFFERS {
            empty_tx
                .send(Vec::with_capacity(WRITER_BUFFER_BYTES))
                .map_err(|e| e.to_string())?;
        }
        let thread = std::thread::Builder::new()
            .name("h264-writer".into())
            .spawn(move || {
                let mut out = BufWriter::with_capacity(4 << 20, file);
                let mut written = 0u64;
                for mut buffer in full_rx {
                    out.write_all(&buffer)?;
                    written += buffer.len() as u64;
                    buffer.clear();
                    if empty_tx.send(buffer).is_err() {
                        break;
                    }
                }
                out.flush()?;
                Ok(written)
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            full: Some(full_tx),
            empty: empty_rx,
            thread: Some(thread),
        })
    }

    /// Hands a copy of the packet to the writer. Returns true if it had to wait for a free buffer.
    fn write(&mut self, data: &[u8]) -> Result<bool, String> {
        let (mut buffer, stalled) = match self.empty.try_recv() {
            Ok(buffer) => (buffer, false),
            Err(TryRecvError::Empty) => (self.empty.recv().map_err(|e| e.to_string())?, true),
            Err(TryRecvError::Disconnected) => return Err("the writer thread stopped".into()),
        };
        buffer.extend_from_slice(data);
        self.full
            .as_ref()
            .ok_or("writer closed")?
            .send(buffer)
            .map_err(|_| "the writer thread stopped".to_string())?;
        Ok(stalled)
    }

    fn finish(mut self) -> Result<u64, String> {
        drop(self.full.take());
        let thread = self.thread.take().ok_or("writer already finished")?;
        thread
            .join()
            .map_err(|_| "the writer thread panicked".to_string())?
            .map_err(|e| format!("cannot write the stream: {e}"))
    }
}

struct Session {
    encoder: Box<dyn Encoder>,
    started: Instant,
    last_pts: i64,
    force_idr: bool,
    packet: EncodedFrame,
    writer: Writer,
    stages: Stages,
    counters: Counters,
    first_sps: Option<h264::Sps>,
    first_nal_types: Vec<u8>,
    frame_interval_ms: f64,
}

impl Session {
    fn submit(
        &mut self,
        frame: &CapturedFrame<'_>,
        capture_wait_ms: Option<f64>,
    ) -> Result<(), String> {
        // pts in 100 ns units from the start, strictly growing also for repeated frames.
        let pts = ((frame.acquired - self.started).as_nanos() / 100) as i64;
        let pts = pts.max(self.last_pts + 1);
        self.last_pts = pts;

        let force_idr = std::mem::take(&mut self.force_idr);
        let submit_started = Instant::now();
        let stats = self
            .encoder
            .encode(frame, pts, force_idr)
            .map_err(|e| format!("encode: {e}"))?;
        let submit_ms = submit_started.elapsed().as_secs_f64() * 1000.0;
        if force_idr {
            self.counters.forced_idr += 1;
        }
        if stats.input_in_video_memory {
            self.counters.input_in_video_memory += 1;
        } else {
            self.counters.input_elsewhere += 1;
        }
        if stats.converted_in_video_memory {
            self.counters.converted_in_video_memory += 1;
        } else {
            self.counters.converted_elsewhere += 1;
        }
        self.counters.input_full += u64::from(stats.input_full);

        // Low Latency gives one output per input; wait for it so the encode time belongs to this frame.
        let got = loop {
            if self
                .encoder
                .poll(&mut self.packet)
                .map_err(|e| format!("poll: {e}"))?
            {
                break true;
            }
            if submit_started.elapsed() > OUTPUT_TIMEOUT {
                break false;
            }
        };
        let done = Instant::now();
        if !got {
            self.counters.lost_outputs += 1;
            self.force_idr = true;
            return Ok(());
        }
        if self.packet.pts != pts {
            self.counters.pts_mismatch += 1;
        }

        let encode_ms = (done - submit_started).as_secs_f64() * 1000.0 - stats.convert_ms;
        let total_ms = (done - frame.acquired).as_secs_f64() * 1000.0;
        if let Some(wait) = capture_wait_ms {
            self.stages.capture_wait.push(wait);
        }
        if let Some(v) = frame.present_to_acquire_ms {
            self.stages.present_to_acquire.push(v);
        }
        if !frame.repeated {
            self.stages.copy.push(frame.copy_ms);
        }
        self.stages.convert.push(stats.convert_ms);
        self.stages.submit.push(submit_ms);
        self.stages.encode.push(encode_ms);
        self.stages.total.push(total_ms);
        self.stages.frame_bytes.push(self.packet.data.len() as f64);
        if total_ms > self.frame_interval_ms {
            self.counters.late_frames += 1;
        }

        self.counters.encoded += 1;
        self.counters.bytes += self.packet.data.len() as u64;
        self.counters.largest = self.counters.largest.max(self.packet.data.len());
        if self.packet.keyframe {
            self.counters.idr += 1;
            if self.first_sps.is_none() {
                for nal in h264::nal_units(&self.packet.data) {
                    if let Some(t) = h264::nal_type(nal) {
                        self.first_nal_types.push(t);
                    }
                    if self.first_sps.is_none() {
                        self.first_sps = h264::parse_sps(nal);
                    }
                }
            }
        }
        if self.writer.write(&self.packet.data)? {
            self.counters.writer_stalls += 1;
        }
        Ok(())
    }
}

fn encoder_config(options: &Options, input: (u32, u32)) -> EncoderConfig {
    let e = &options.encode;
    let mut config = EncoderConfig::low_latency(e.width, e.height, e.fps, e.bitrate);
    config.input_width = input.0 as i32;
    config.input_height = input.1 as i32;
    if let Some(kbit) = options.vbv_kbit {
        config.vbv_bits = kbit * 1000;
    }
    if let Some(ms) = options.query_timeout_ms {
        config.query_timeout_ms = ms;
    }
    if let Some(method) = options.rate_control {
        config.rate_control = method;
    }
    config
}

fn sps_report(sps: &Option<h264::Sps>) -> Value {
    let Some(sps) = sps else {
        return Value::Null;
    };
    let vui = sps.vui.clone().unwrap_or_default();
    json!({
        "profile_idc": sps.profile_idc,
        "constraint_flags": sps.constraint_flags,
        "level_idc": sps.level_idc,
        "width": sps.width,
        "height": sps.height,
        "chroma_format_idc": sps.chroma_format_idc,
        "bit_depth_luma": sps.bit_depth_luma,
        "pic_order_cnt_type": sps.pic_order_cnt_type,
        "max_num_ref_frames": sps.max_num_ref_frames,
        "vui_present": sps.vui.is_some(),
        "video_full_range": vui.video_full_range,
        "colour_primaries": vui.colour_primaries,
        "transfer_characteristics": vui.transfer_characteristics,
        "matrix_coefficients": vui.matrix_coefficients,
        "timing": vui.timing.map(|(u, s, fixed)| json!({ "num_units_in_tick": u, "time_scale": s, "fixed": fixed })),
        "max_num_reorder_frames": vui.max_num_reorder_frames,
        "max_dec_frame_buffering": vui.max_dec_frame_buffering,
        "bt709_limited": vui.video_full_range == Some(false)
            && vui.colour_primaries == Some(1)
            && vui.transfer_characteristics == Some(1)
            && vui.matrix_coefficients == Some(1),
    })
}

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    logging::init(&options.out.join("rcs-bench.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "capture").entered();
    set_dpi_awareness();
    let report = measure(options)?;
    match report["failure"].as_str() {
        Some(failure) => Err(failure.to_string()),
        None => Ok(()),
    }
}

/// Real pixel coordinates for the whole process. Call once: a second call fails with access denied.
pub fn set_dpi_awareness() {
    if let Err(error) = dxgi::set_dpi_awareness() {
        warn!(error = %dxgi::describe(&error), "dpi awareness");
    }
}

fn open_capture(
    adapter: &IDXGIAdapter1,
    options: &Options,
) -> Result<(ID3D11Device, DxgiCapture), String> {
    let (device, _) = dxgi::create_device(adapter).map_err(|e| dxgi::describe(&e))?;
    let capture = DxgiCapture::new(adapter.clone(), device.clone(), options.display)
        .map_err(|e| e.to_string())?;
    Ok((device, capture))
}

/// After the GPU was removed or reset: a new adapter object, device, capture and encoder, as the agent will do.
fn rebuild(options: &Options) -> Result<(ID3D11Device, DxgiCapture, Box<dyn Encoder>), String> {
    let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
    let adapter = probe::encoder_adapter(&adapters).ok_or("no hardware adapter after the reset")?;
    let (device, capture) = open_capture(&adapter.adapter, options)?;
    let config = encoder_config(options, capture.size());
    let encoder = rcs_encode::open(options.backend, &device, &config)
        .map_err(|e| format!("encoder after the reset: {e}"))?;
    Ok((device, capture, encoder))
}

/// True when the device is gone and only a rebuild helps.
fn device_lost(device: &ID3D11Device) -> bool {
    // SAFETY: the device is valid; the call only reads its state.
    unsafe { device.GetDeviceRemovedReason() }.is_err()
}

/// One capture run into `options.out`. A failure during the run is recorded in the report's `failure`;
/// `Err` means the run could not start or the report could not be written.
pub fn measure(options: &Options) -> Result<Value, String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    info!(?options, "capture started");

    let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
    let adapter = probe::encoder_adapter(&adapters).ok_or("no hardware adapter")?;
    let (mut device, mut capture) = open_capture(&adapter.adapter, options)?;
    let mut input_size = capture.size();
    let config = encoder_config(options, input_size);
    let backend = options.backend;
    let encoder = rcs_encode::open(backend, &device, &config)
        .map_err(|e| format!("encoder {}: {e}", backend.name()))?;
    info!(
        requested = backend.name(),
        actual = encoder.backend(),
        "encoder opened"
    );
    let settings: Value = encoder
        .settings()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);

    let fps = f64::from(options.encode.fps.max(1));
    let frame_interval = Duration::from_secs_f64(1.0 / fps);
    let repeat_interval = (options.repeat_fps > 0)
        .then(|| Duration::from_secs_f64(1.0 / f64::from(options.repeat_fps)));
    let duration = Duration::from_secs(u64::from(options.seconds));
    let expected = (duration.as_secs_f64() * fps) as usize;

    println!(
        "Захват {}x{} с {}, кодирование {}x{} @ {} к/с, {} Мбит/с, {} с...",
        input_size.0,
        input_size.1,
        adapter.info.name,
        options.encode.width,
        options.encode.height,
        options.encode.fps,
        options.encode.bitrate / 1_000_000,
        options.seconds
    );

    let timer = TimerResolution::new(1);
    let started = Instant::now();
    let mut session = Session {
        encoder,
        started,
        last_pts: -1,
        force_idr: false,
        packet: EncodedFrame {
            data: Vec::with_capacity(WRITER_BUFFER_BYTES),
            ..Default::default()
        },
        writer: Writer::start(&options.out.join(STREAM_FILE))?,
        stages: Stages::with_capacity(expected + expected / 4),
        counters: Counters {
            encoders_created: 1,
            ..Default::default()
        },
        first_sps: None,
        first_nal_types: Vec::new(),
        frame_interval_ms: 1000.0 / fps,
    };
    let mut last_submit = started;
    let mut failure: Option<String> = None;
    let mut simulated_loss = false;
    let mut credit = 1.0f64;
    let mut credit_at = started;

    while started.elapsed() < duration {
        // A source faster than the stream (a game without vsync) is sampled at the stream rate: the next new frame
        // is taken no earlier than PACING_SHARE of an interval after the previous one. Presents merged meanwhile
        // are dropped on purpose, not missed.
        // Credit grows at the stream rate up to one frame; a frame needs PACING_SHARE of credit and costs one, so the
        // long-run rate is exactly the stream rate while a source at the display rate never waits.
        let mut paced = false;
        let now = Instant::now();
        credit = (credit + (now - credit_at).as_secs_f64() * fps).min(1.0);
        credit_at = now;
        if credit < PACING_SHARE {
            std::thread::sleep(Duration::from_secs_f64((PACING_SHARE - credit) / fps));
            paced = true;
            let now = Instant::now();
            credit = (credit + (now - credit_at).as_secs_f64() * fps).min(1.0);
            credit_at = now;
        }
        let wait_started = Instant::now();
        let mut repeat_due = false;
        let simulate_now = !simulated_loss
            && options
                .simulate_device_lost_at
                .is_some_and(|at| started.elapsed() >= Duration::from_secs(u64::from(at)));
        if simulate_now {
            simulated_loss = true;
            warn!("simulated device loss");
        }
        let mut rebuild_needed = simulate_now;
        if !simulate_now {
            // Wait for a new frame, but not past the moment the last frame is due again.
            let wait = match repeat_interval {
                Some(r) if capture.last_frame().is_some() => r
                    .saturating_sub(last_submit.elapsed())
                    .clamp(Duration::from_millis(1), frame_interval),
                _ => frame_interval,
            };
            match capture.next_frame(wait) {
                Ok(CaptureEvent::Frame(frame)) => {
                    let wait_ms = (frame.acquired - wait_started).as_secs_f64() * 1000.0;
                    session.counters.captured += 1;
                    let merged = frame.accumulated_frames.saturating_sub(1);
                    // The first frame carries everything presented before the duplication started.
                    if merged > 0 && session.counters.captured > 1 {
                        if paced {
                            session.counters.merged_by_pacing += u64::from(merged);
                        } else if wait_ms < PENDING_FRAME_MS {
                            session.counters.merged_while_busy += u64::from(merged);
                        } else {
                            session.counters.merged_while_waiting += u64::from(merged);
                        }
                        if session.counters.merges.len() < MERGES_KEPT {
                            let at = session.counters.captured;
                            session.counters.merges.push((at, merged, wait_ms));
                        }
                    }
                    last_submit = Instant::now();
                    credit -= 1.0;
                    if let Err(e) = session.submit(&frame, Some(wait_ms)) {
                        if device_lost(&device) {
                            warn!(error = %e, "device lost while encoding");
                            rebuild_needed = true;
                        } else {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                Ok(CaptureEvent::Timeout) => {
                    repeat_due = repeat_interval.is_some_and(|r| last_submit.elapsed() >= r);
                }
                Ok(CaptureEvent::Reinitialize) => {
                    session.counters.reinitialized += 1;
                    session.force_idr = true;
                    let size = capture.size();
                    if size != input_size && size.0 > 0 {
                        warn!(?size, "display size changed, recreating the encoder");
                        match rcs_encode::open(backend, &device, &encoder_config(options, size)) {
                            Ok(encoder) => {
                                session.encoder = encoder;
                                session.counters.encoders_created += 1;
                                input_size = size;
                            }
                            Err(e) => {
                                failure = Some(format!("encoder after a mode change: {e}"));
                                break;
                            }
                        }
                    }
                }
                Err(CaptureError::DeviceRemoved(e)) => {
                    warn!(error = %dxgi::describe(&e), "device lost while capturing");
                    rebuild_needed = true;
                }
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
        if rebuild_needed {
            let rebuild_started = Instant::now();
            capture.release();
            match rebuild(options) {
                Ok((new_device, mut new_capture, new_encoder)) => {
                    new_capture.counters.absorb(&capture.counters);
                    device = new_device;
                    capture = new_capture;
                    session.encoder = new_encoder;
                    input_size = capture.size();
                    session.force_idr = true;
                    session.counters.device_rebuilds += 1;
                    session.counters.encoders_created += 1;
                    let ms = rebuild_started.elapsed().as_secs_f64() * 1000.0;
                    session.counters.rebuild_ms.push(ms);
                    info!(ms, "pipeline rebuilt");
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
            continue;
        }
        if repeat_due && let Some(frame) = capture.last_frame() {
            session.counters.repeated += 1;
            last_submit = Instant::now();
            if let Err(e) = session.submit(&frame, None) {
                failure = Some(e);
                break;
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let sps_rewritten = session.encoder.sps_rewritten();
    let backend_used = session.encoder.backend();

    let Session {
        writer,
        stages,
        counters: c,
        first_sps,
        first_nal_types,
        ..
    } = session;
    let written = writer.finish()?;
    let encoded_fps = c.encoded as f64 / elapsed.max(1e-9);
    let report = json!({
        "tool": "rcs-bench capture",
        "build": BUILD_ID,
        "adapter": adapter.info.name,
        "encoder": { "requested": backend.name(), "backend": backend_used },
        "driver": adapter.info.driver_version,
        "failure": failure,
        "options": {
            "seconds": options.seconds,
            "display": options.display,
            "width": options.encode.width,
            "height": options.encode.height,
            "fps": options.encode.fps,
            "bitrate": options.encode.bitrate,
            "repeat_fps": options.repeat_fps,
            "vbv_kbit": options.vbv_kbit,
            "query_timeout_ms": options.query_timeout_ms,
            "rate_control": config.rate_control,
        },
        "input": { "width": input_size.0, "height": input_size.1 },
        "display": { "refresh_hz": (capture.refresh_hz() * 1000.0).round() / 1000.0 },
        "encoder_settings": settings,
        "timer_resolution_ms": timer.as_ref().map(TimerResolution::period_ms),
        "elapsed_s": (elapsed * 1000.0).round() / 1000.0,
        "frames": {
            "expected_at_fps": expected,
            "captured": c.captured,
            "repeated": c.repeated,
            "encoded": c.encoded,
            "encoded_fps": (encoded_fps * 100.0).round() / 100.0,
            "desktop_presents_merged": capture.counters.accumulated_extra,
            "merged_while_busy": c.merged_while_busy,
            "merged_while_waiting": c.merged_while_waiting,
            "merged_by_pacing": c.merged_by_pacing,
            "merges": c.merges.iter().map(|&(at, n, wait)| json!({
                "frame": at, "merged": n, "wait_ms": (wait * 1000.0).round() / 1000.0,
            })).collect::<Vec<_>>(),
            "late_frames": c.late_frames,
            "lost_outputs": c.lost_outputs,
            "pts_mismatch": c.pts_mismatch,
            "idr": c.idr,
            "forced_idr": c.forced_idr,
            "input_full": c.input_full,
        },
        "capture": {
            "timeouts": capture.counters.timeouts,
            "pointer_only": capture.counters.pointer_only,
            "access_lost": capture.counters.access_lost,
            "reinitialized": c.reinitialized,
            "pool_recreated": capture.counters.pool_recreated,
            "reopen_failures": capture.counters.reopen_failures,
            "last_reopen_error": capture.counters.last_reopen_error,
            "encoders_created": c.encoders_created,
            "device_rebuilds": c.device_rebuilds,
            "rebuild_ms": c.rebuild_ms.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<_>>(),
            "simulated_device_loss_at_s": options.simulate_device_lost_at,
        },
        "memory": {
            "desktop_in_system_memory": capture.memory.desktop_in_system_memory,
            "pool_usage_default": capture.memory.pool_usage == 0,
            "pool_cpu_access_flags": capture.memory.pool_cpu_access_flags,
            "pool_bind_flags": capture.memory.pool_bind_flags,
            "pool_format": capture.memory.format,
            "amf_input_dx11_frames": c.input_in_video_memory,
            "amf_input_other_frames": c.input_elsewhere,
            "amf_nv12_dx11_frames": c.converted_in_video_memory,
            "amf_nv12_other_frames": c.converted_elsewhere,
            "cpu_maps": 0,
            "frame_stays_in_video_memory": !capture.memory.desktop_in_system_memory
                && capture.memory.pool_cpu_access_flags == 0
                && c.input_elsewhere == 0
                && c.converted_elsewhere == 0
                && c.encoded > 0,
        },
        "stream": {
            "file": STREAM_FILE,
            "bytes": written,
            "largest_frame": c.largest,
            "bitrate_mbps": ((c.bytes as f64 * 8.0 / elapsed.max(1e-9) / 1e4).round()) / 100.0,
            "writer_stalls": c.writer_stalls,
            "first_idr_nal_types": first_nal_types,
            "sps_colour_rewritten": sps_rewritten,
            "sps": sps_report(&first_sps),
        },
        "stages": stages.report(),
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    info!(encoded = c.encoded, "capture finished");

    print_summary(&report);
    println!("Отчёт: {}", path.display());
    Ok(report)
}

fn print_summary(r: &Value) {
    let f = &r["frames"];
    println!(
        "Кадров: захвачено {}, повторов {}, закодировано {} ({} к/с), IDR {}, опоздавших {}, потерянных выходов {}",
        f["captured"],
        f["repeated"],
        f["encoded"],
        f["encoded_fps"],
        f["idr"],
        f["late_frames"],
        f["lost_outputs"]
    );
    for (key, name) in [
        ("present_to_acquire_ms", "present→захват"),
        ("convert_ms", "BGRA→NV12"),
        ("encode_ms", "кодирование"),
        ("total_ms", "захват→пакет"),
    ] {
        let s = &r["stages"][key]["summary"];
        if s.is_null() {
            continue;
        }
        println!(
            "  {name}: p50 {} мс, p95 {} мс, p99 {} мс, максимум {} мс",
            s["p50"], s["p95"], s["p99"], s["max"]
        );
    }
    let sps = &r["stream"]["sps"];
    println!(
        "Поток: {} Мбит/с, SPS profile {} level {}, {}x{}, BT.709 limited: {}, max_num_reorder_frames {}",
        r["stream"]["bitrate_mbps"],
        sps["profile_idc"],
        sps["level_idc"],
        sps["width"],
        sps["height"],
        sps["bt709_limited"],
        sps["max_num_reorder_frames"]
    );
    println!(
        "Кадр в видеопамяти на всём пути: {}",
        r["memory"]["frame_stays_in_video_memory"]
    );
}
