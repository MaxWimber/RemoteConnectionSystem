//! `rcs-bench matrix`: AMF settings one at a time against fixed clips, with timings, frame sizes and quality.
//!
//! The capture is not involved. ffmpeg decodes a clip to BGRA, each frame is uploaded into a GPU texture and goes
//! through the same `AmfEncoder` as in the capture pipeline, paced at the clip frame rate. Every encoded frame belongs
//! to one clip frame, so ffmpeg can compare the result with the source frame by frame (SSIM and PSNR in RGB).
//!
//! Clips, made once with ffmpeg and kept in the output folder, both 4:4:4 so the reference keeps full chroma:
//! - `video`: moving gradients, a moving test card and film-like noise, the hardest case for the rate control.
//! - `text`: source code scrolling on a light background under a title bar, like an editor or a web page.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_capture::{CapturedFrame, TimerResolution, dxgi};
use rcs_encode::amf::{self, AmfEncoder, EncoderConfig};
use rcs_encode::{EncodedFrame, Encoder};
use rcs_protocol::BUILD_ID;
use serde_json::{Value, json};
use tracing::{info, info_span, warn};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::capture;
use crate::logging;
use crate::options::Options;
use crate::probe;
use crate::scenarios::{Tools, find_tools};
use crate::stats::summarize;

const REPORT_FILE: &str = "matrix.json";
const CLIP_WIDTH: u32 = 1920;
const CLIP_HEIGHT: u32 = 1080;
const CLIP_FPS: u32 = 60;
const UPLOAD_TEXTURES: usize = 4;
const READER_BUFFERS: usize = 4;
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipKind {
    Video,
    Text,
}

impl ClipKind {
    fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Text => "text",
        }
    }
}

struct Variant {
    name: &'static str,
    what: &'static str,
    config: EncoderConfig,
}

/// One change from the baseline per variant, so each effect stays visible.
fn variants(fps: i32) -> Vec<Variant> {
    let mbps = |m: i64| m * 1_000_000;
    let base = EncoderConfig::low_latency(CLIP_WIDTH as i32, CLIP_HEIGHT as i32, fps, mbps(30));
    let frame_bits = base.bitrate / i64::from(fps);
    let with = |name, what, change: &dyn Fn(&mut EncoderConfig)| {
        let mut config = base;
        change(&mut config);
        Variant { name, what, config }
    };
    vec![
        with(
            "baseline",
            "Low Latency, LCVBR 30 Мбит/с (D-018), High, остальное по умолчанию",
            &|_| {},
        ),
        with("usage_webcam", "Usage Webcam", &|c| {
            c.usage = amf::USAGE_WEBCAM
        }),
        with("usage_transcoding", "Usage Transcoding", &|c| {
            c.usage = amf::USAGE_TRANSCODING
        }),
        with("preset_speed", "QualityPreset Speed", &|c| {
            c.quality_preset = amf::QUALITY_PRESET_SPEED
        }),
        with("preset_balanced", "QualityPreset Balanced", &|c| {
            c.quality_preset = amf::QUALITY_PRESET_BALANCED
        }),
        with("preset_quality", "QualityPreset Quality", &|c| {
            c.quality_preset = amf::QUALITY_PRESET_QUALITY
        }),
        with("vbv_1_frame", "VBV на 1 кадр", &|c| {
            c.vbv_bits = frame_bits
        }),
        with("vbv_2_frames", "VBV на 2 кадра", &|c| {
            c.vbv_bits = 2 * frame_bits
        }),
        with("vbv_1_frame_hrd", "VBV на 1 кадр, EnforceHRD", &|c| {
            c.vbv_bits = frame_bits;
            c.enforce_hrd = 1;
        }),
        with(
            "vbv_ull_defaults",
            "VBV 735 000 бит и EnforceHRD, как у Ultra Low Latency",
            &|c| {
                c.vbv_bits = 735_000;
                c.enforce_hrd = 1;
            },
        ),
        with("max_au_2_frames", "MaxAUSize на 2 кадра", &|c| {
            c.max_au_bits = 2 * frame_bits
        }),
        with(
            "intra_refresh_off",
            "Intra-refresh выключен",
            &|c| c.intra_refresh_mbs = 0,
        ),
        with(
            "intra_refresh_continuous",
            "Intra-refresh непрерывный, 255 MB на кадр (полный кадр за 32 кадра)",
            &|c| {
                c.intra_refresh_mode = 2;
                c.intra_refresh_mbs = 255;
            },
        ),
        with(
            "intra_refresh_gop",
            "Intra-refresh по GOP, 255 MB на кадр",
            &|c| {
                c.intra_refresh_mode = 1;
                c.intra_refresh_mbs = 255;
            },
        ),
        with("vbaq_on", "VBAQ включён", &|c| c.vbaq = 1),
        with("filler_on", "Filler data включён", &|c| {
            c.filler_data = 1
        }),
        with("cbr", "CBR, как до D-018", &|c| {
            c.rate_control = amf::RATE_CONTROL_CBR
        }),
        with(
            "cbr_vbv_1_frame_hrd",
            "CBR, VBV на 1 кадр, EnforceHRD",
            &|c| {
                c.rate_control = amf::RATE_CONTROL_CBR;
                c.vbv_bits = frame_bits;
                c.enforce_hrd = 1;
            },
        ),
        with(
            "profile_constrained_baseline",
            "Profile Constrained Baseline",
            &|c| c.profile = amf::PROFILE_CONSTRAINED_BASELINE,
        ),
        with(
            "query_timeout_0",
            "QueryTimeout 0, активный опрос",
            &|c| c.query_timeout_ms = 0,
        ),
        with(
            "mode_desktop_40",
            "Режим «Рабочий стол»: 40 Мбит/с",
            &|c| c.bitrate = mbps(40),
        ),
        with(
            "mode_weak_720p_10",
            "Режим «Слабая сеть»: 1280×720, 10 Мбит/с",
            &|c| {
                c.width = 1280;
                c.height = 720;
                c.bitrate = mbps(10);
            },
        ),
    ]
}

/// The clip for `kind`, made once. 4:4:4 keeps the reference chroma sharp.
fn ensure_clip(tools: &Tools, out: &Path, kind: ClipKind, seconds: u32) -> Result<PathBuf, String> {
    let clip = out.join(format!("matrix-{}-{seconds}s.mp4", kind.name()));
    if clip.is_file() {
        return Ok(clip);
    }
    println!("Готовлю ролик {} на {seconds} с...", kind.name());
    let d = seconds;
    let graph = match kind {
        ClipKind::Video => format!(
            "gradients=s=1920x1080:r=60:n=6:speed=0.02:d={d}[g];testsrc2=s=960x540:r=60:d={d}[t];\
             [g][t]overlay=x='(W-w)/2+(W-w)/2*sin(t*1.3)':y='(H-h)/2+(H-h)/2*cos(t*0.9)':shortest=1,\
             noise=alls=6:allf=t,format=yuv444p[v]"
        ),
        ClipKind::Text => {
            // Our own source code as the text: long, varied lines, no network or fixtures needed.
            let text = include_str!("capture.rs");
            fs::write(out.join("matrix-text.txt"), text)
                .map_err(|e| format!("cannot write the clip text: {e}"))?;
            format!(
                "color=c=0xF3F3F3:s=1920x1080:r=60:d={d},\
                 drawtext=fontfile='C\\:/Windows/Fonts/consola.ttf':textfile=matrix-text.txt:expansion=none:\
                 fontsize=20:fontcolor=0x202020:line_spacing=4:x=60:y=60-t*240,\
                 drawtext=fontfile='C\\:/Windows/Fonts/consola.ttf':textfile=matrix-text.txt:expansion=none:\
                 fontsize=16:fontcolor=0x1040C0:line_spacing=3:x=1000:y=60-t*120,\
                 drawbox=x=0:y=0:w=iw:h=48:color=0x2B579A:t=fill,format=yuv444p[v]"
            )
        }
    };
    let partial = out.join(format!("matrix-{}.partial.mp4", kind.name()));
    let status = Command::new(&tools.ffmpeg)
        .current_dir(out)
        .args([
            "-v",
            "error",
            "-y",
            "-filter_complex",
            &graph,
            "-map",
            "[v]",
        ])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "12", "-g", "60", "-an",
        ])
        .args(["-f", "mp4"])
        .arg(&partial)
        .status()
        .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!(
            "ffmpeg could not make the {} clip: {status}",
            kind.name()
        ));
    }
    fs::rename(&partial, &clip).map_err(|e| format!("cannot keep the clip: {e}"))?;
    Ok(clip)
}

/// BGRA frames of a clip, decoded by ffmpeg on a reader thread into recycled buffers.
struct ClipReader {
    child: Child,
    full: Receiver<Vec<u8>>,
    empty: SyncSender<Vec<u8>>,
    thread: Option<JoinHandle<()>>,
}

impl ClipReader {
    fn open(tools: &Tools, clip: &Path, frames: u32) -> Result<Self, String> {
        let mut child = Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(clip)
            .args([
                "-frames:v",
                &frames.to_string(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
        let stdout = child.stdout.take().ok_or("ffmpeg has no stdout")?;
        let frame_bytes = (CLIP_WIDTH * CLIP_HEIGHT * 4) as usize;
        let (full_tx, full_rx) = sync_channel::<Vec<u8>>(READER_BUFFERS);
        let (empty_tx, empty_rx) = sync_channel::<Vec<u8>>(READER_BUFFERS);
        for _ in 0..READER_BUFFERS {
            empty_tx
                .send(vec![0; frame_bytes])
                .map_err(|e| e.to_string())?;
        }
        let thread = std::thread::Builder::new()
            .name("clip-reader".into())
            .spawn(move || read_frames(stdout, empty_rx, full_tx))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            child,
            full: full_rx,
            empty: empty_tx,
            thread: Some(thread),
        })
    }

    fn next(&self) -> Option<Vec<u8>> {
        self.full.recv().ok()
    }

    fn give_back(&self, buffer: Vec<u8>) {
        let _ = self.empty.send(buffer);
    }
}

fn read_frames(mut stdout: ChildStdout, empty: Receiver<Vec<u8>>, full: SyncSender<Vec<u8>>) {
    for mut buffer in empty {
        if stdout.read_exact(&mut buffer).is_err() || full.send(buffer).is_err() {
            return;
        }
    }
}

impl Drop for ClipReader {
    fn drop(&mut self) {
        // The reader thread ends on its own: its read fails once ffmpeg is gone, and its channels close when the
        // fields of this struct drop right after this. Joining here could wait on a buffer that never comes back.
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.thread.take());
    }
}

fn upload_textures(device: &ID3D11Device) -> Result<Vec<ID3D11Texture2D>, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: CLIP_WIDTH,
        Height: CLIP_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    (0..UPLOAD_TEXTURES)
        .map(|_| {
            let mut texture = None;
            // SAFETY: the descriptor and the out pointer are live locals; no initial data.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(|e| dxgi::describe(&e))?;
            texture.ok_or_else(|| "CreateTexture2D returned nothing".to_string())
        })
        .collect()
}

#[derive(Default)]
struct RunStats {
    upload: Vec<f64>,
    convert: Vec<f64>,
    encode: Vec<f64>,
    frame_bytes: Vec<f64>,
    late_submits: u64,
    lost_outputs: u64,
    idr: u64,
    input_full: u64,
    encoded: u64,
}

/// Encodes `frames` clip frames with `config` into `stream`, paced at the clip frame rate.
fn encode_clip(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    textures: &[ID3D11Texture2D],
    reader: &ClipReader,
    config: &EncoderConfig,
    frames: u32,
    stream: &mut Vec<u8>,
) -> Result<(RunStats, Value), String> {
    let mut encoder = AmfEncoder::new(device, config).map_err(|e| format!("AMF encoder: {e}"))?;
    let settings: Value = encoder
        .settings()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let interval = Duration::from_secs_f64(1.0 / f64::from(CLIP_FPS));
    let mut stats = RunStats::default();
    let mut packet = EncodedFrame::default();
    let started = Instant::now();
    for i in 0..frames {
        let Some(buffer) = reader.next() else {
            return Err(format!("the clip ended after {i} frames"));
        };
        let texture = &textures[i as usize % textures.len()];
        let upload_started = Instant::now();
        // SAFETY: the buffer holds one full BGRA frame of the texture size with the given row pitch.
        unsafe {
            context.UpdateSubresource(texture, 0, None, buffer.as_ptr().cast(), CLIP_WIDTH * 4, 0)
        };
        stats
            .upload
            .push(upload_started.elapsed().as_secs_f64() * 1000.0);
        reader.give_back(buffer);

        let due = started + interval * i;
        let now = Instant::now();
        if now > due + interval {
            stats.late_submits += 1;
        } else if due > now {
            std::thread::sleep(due - now);
        }
        let frame = CapturedFrame {
            texture,
            width: CLIP_WIDTH,
            height: CLIP_HEIGHT,
            acquired: Instant::now(),
            present_to_acquire_ms: None,
            accumulated_frames: 1,
            copy_ms: 0.0,
            repeated: false,
        };
        let pts = i64::from(i) * 10_000_000 / i64::from(CLIP_FPS);
        let submit_started = Instant::now();
        let submit = encoder
            .encode(&frame, pts, false)
            .map_err(|e| format!("encode: {e}"))?;
        stats.input_full += u64::from(submit.input_full);
        let got = loop {
            if encoder
                .poll(&mut packet)
                .map_err(|e| format!("poll: {e}"))?
            {
                break true;
            }
            if submit_started.elapsed() > OUTPUT_TIMEOUT {
                break false;
            }
        };
        if !got {
            stats.lost_outputs += 1;
            continue;
        }
        let total_ms = submit_started.elapsed().as_secs_f64() * 1000.0;
        stats.convert.push(submit.convert_ms);
        stats.encode.push(total_ms - submit.convert_ms);
        stats.frame_bytes.push(packet.data.len() as f64);
        stats.encoded += 1;
        if packet.keyframe {
            stats.idr += 1;
        }
        stream.extend_from_slice(&packet.data);
    }
    // Late outputs of the last frames, if any, still belong to the stream.
    let drain_until = Instant::now() + OUTPUT_TIMEOUT;
    while Instant::now() < drain_until {
        if encoder
            .poll(&mut packet)
            .map_err(|e| format!("poll: {e}"))?
        {
            stream.extend_from_slice(&packet.data);
            stats.encoded += 1;
        } else {
            break;
        }
    }
    Ok((stats, settings))
}

/// Mean, minimum and the 1st percentile of one per-frame quality value.
fn quality_summary(values: &mut [f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(f64::total_cmp);
    let round = |v: f64| (v * 10_000.0).round() / 10_000.0;
    let p1 = values[(values.len() / 100).min(values.len() - 1)];
    json!({
        "frames": values.len(),
        "mean": round(values.iter().sum::<f64>() / values.len() as f64),
        "p1": round(p1),
        "min": round(values[0]),
    })
}

/// Reads `key:value` from every line of an ffmpeg stats file.
fn stats_values(path: &Path, key: &str) -> Vec<f64> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let prefix = format!("{key}:");
    text.lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix(prefix.as_str()))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .filter(|v| v.is_finite())
        .collect()
}

/// SSIM and PSNR of the encoded stream against the clip, frame by frame, in RGB.
fn measure_quality(
    tools: &Tools,
    dir: &Path,
    stream: &Path,
    clip: &Path,
    config: &EncoderConfig,
    frames: u32,
) -> Value {
    let scale = if (config.width, config.height) != (CLIP_WIDTH as i32, CLIP_HEIGHT as i32) {
        format!("scale={}:{}:flags=bicubic,", config.width, config.height)
    } else {
        String::new()
    };
    // Both sides get frame numbers as timestamps, so frame i of the stream meets frame i of the clip.
    let graph = format!(
        "[0:v]settb=1/60,setpts=N,format=bgra,format=gbrp,split[a1][a2];\
         [1:v]trim=end_frame={frames},settb=1/60,setpts=N,{scale}format=bgra,format=gbrp,split[b1][b2];\
         [a1][b1]ssim=stats_file=ssim.log;[a2][b2]psnr=stats_file=psnr.log"
    );
    let status = Command::new(&tools.ffmpeg)
        .current_dir(dir)
        .args(["-v", "error", "-nostats", "-i"])
        .arg(stream)
        .arg("-i")
        .arg(clip)
        .args(["-filter_complex", &graph, "-f", "null", "-"])
        .status();
    match status {
        Ok(s) if s.success() => {
            let mut ssim = stats_values(&dir.join("ssim.log"), "All");
            let mut psnr = stats_values(&dir.join("psnr.log"), "psnr_avg");
            json!({ "ssim": quality_summary(&mut ssim), "psnr_db": quality_summary(&mut psnr) })
        }
        Ok(s) => json!({ "error": format!("ffmpeg: {s}") }),
        Err(e) => json!({ "error": format!("cannot start ffmpeg: {e}") }),
    }
}

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    logging::init(&options.out.join("rcs-bench.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "matrix").entered();
    capture::set_dpi_awareness();
    let tools = find_tools(options)?;
    // ffmpeg runs in other folders, so every path it gets is absolute.
    let out = std::path::absolute(&options.out).map_err(|e| e.to_string())?;
    let seconds = options.seconds.max(2);
    let frames = seconds * CLIP_FPS;
    let clips = [ClipKind::Video, ClipKind::Text]
        .into_iter()
        .map(|k| ensure_clip(&tools, &out, k, seconds).map(|p| (k, p)))
        .collect::<Result<Vec<_>, _>>()?;

    let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
    let adapter = probe::encoder_adapter(&adapters).ok_or("no hardware adapter")?;
    let (device, _) = dxgi::create_device(&adapter.adapter).map_err(|e| dxgi::describe(&e))?;
    // SAFETY: the device is valid.
    let context = unsafe { device.GetImmediateContext() }.map_err(|e| dxgi::describe(&e))?;
    let textures = upload_textures(&device)?;
    let _timer = TimerResolution::new(1);

    let mut list = variants(CLIP_FPS as i32);
    if !options.scenarios.is_empty() {
        list.retain(|v| options.scenarios.iter().any(|s| s == v.name));
    }
    println!(
        "Матрица: {} вариантов × {} ролика по {seconds} с на {}",
        list.len(),
        clips.len(),
        adapter.info.name
    );

    let mut results = Vec::new();
    let mut stream = Vec::with_capacity(64 << 20);
    for variant in &list {
        for (kind, clip) in &clips {
            let label = format!("{}/{}", variant.name, kind.name());
            let dir = out.join(variant.name).join(kind.name());
            fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            info!(
                variant = variant.name,
                clip = kind.name(),
                "matrix run started"
            );
            stream.clear();
            let reader = ClipReader::open(&tools, clip, frames)?;
            let outcome = encode_clip(
                &device,
                &context,
                &textures,
                &reader,
                &variant.config,
                frames,
                &mut stream,
            );
            drop(reader);
            let (stats, settings) = match outcome {
                Ok(v) => v,
                Err(e) => {
                    warn!(run = %label, error = %e, "matrix run failed");
                    println!("  {label}: ошибка: {e}");
                    results.push(json!({
                        "variant": variant.name, "what": variant.what, "clip": kind.name(), "failure": e,
                    }));
                    continue;
                }
            };
            let stream_path = dir.join("stream.h264");
            fs::write(&stream_path, &stream)
                .map_err(|e| format!("cannot write {}: {e}", stream_path.display()))?;
            let quality =
                measure_quality(&tools, &dir, &stream_path, clip, &variant.config, frames);
            let c = &variant.config;
            let target_frame_bytes = c.bitrate as f64 / 8.0 / f64::from(CLIP_FPS);
            let largest = stats.frame_bytes.iter().copied().fold(0.0, f64::max);
            let bitrate_mbps = stream.len() as f64 * 8.0 / f64::from(seconds) / 1e6;
            let result = json!({
                "variant": variant.name,
                "what": variant.what,
                "clip": kind.name(),
                "config": {
                    "width": c.width, "height": c.height, "bitrate": c.bitrate, "usage": c.usage,
                    "profile": c.profile, "rate_control": c.rate_control, "quality_preset": c.quality_preset,
                    "vbv_bits": c.vbv_bits, "enforce_hrd": c.enforce_hrd, "filler_data": c.filler_data,
                    "intra_refresh_mbs": c.intra_refresh_mbs, "intra_refresh_mode": c.intra_refresh_mode, "vbaq": c.vbaq, "max_au_bits": c.max_au_bits,
                    "query_timeout_ms": c.query_timeout_ms,
                },
                "encoder_settings": settings,
                "frames": {
                    "submitted": frames, "encoded": stats.encoded, "idr": stats.idr,
                    "lost_outputs": stats.lost_outputs, "late_submits": stats.late_submits,
                    "input_full": stats.input_full,
                },
                "bitrate_mbps": (bitrate_mbps * 100.0).round() / 100.0,
                "frame_bytes": summarize(&stats.frame_bytes),
                "largest_frame_in_target_frames": ((largest / target_frame_bytes) * 100.0).round() / 100.0,
                "stages_ms": {
                    "upload": summarize(&stats.upload),
                    "convert": summarize(&stats.convert),
                    "encode": summarize(&stats.encode),
                },
                "quality": quality,
            });
            println!(
                "  {label}: кодирование p50 {} p99 {} макс {} мс, {:.2} Мбит/с, макс. кадр {} целевых, SSIM {} (мин {}), PSNR {} дБ",
                result["stages_ms"]["encode"]["p50"],
                result["stages_ms"]["encode"]["p99"],
                result["stages_ms"]["encode"]["max"],
                bitrate_mbps,
                result["largest_frame_in_target_frames"],
                result["quality"]["ssim"]["mean"],
                result["quality"]["ssim"]["min"],
                result["quality"]["psnr_db"]["mean"],
            );
            results.push(result);
        }
    }

    let report = json!({
        "tool": "rcs-bench matrix",
        "build": BUILD_ID,
        "adapter": adapter.info.name,
        "driver": adapter.info.driver_version,
        "seconds": seconds,
        "fps": CLIP_FPS,
        "runs": results,
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("Отчёт: {}", path.display());
    Ok(())
}
