//! `rcs-bench scenarios`: the capture run of P1-8 for each screen scenario, one after another, with one summary.
//!
//! - `static`: a full-screen still test pattern, the encoder only sees repeats of the last frame.
//! - `window`: a 960x600 window with changing content moves across the screen at the display rate.
//! - `video`: ffplay shows a synthetic 1080p60 clip full screen: moving gradients, a test card and film-like noise.
//! - `switch`: only on request. The moving window, and 6 s into the load the display goes to 1280x720 for 6 s:
//!   `ACCESS_LOST`, a new duplication with another size, a new encoder and an IDR (P1-2).
//!
//! - `gpu`: only on request. A full-screen shader without a frame cap, a stand-in for a game.
//! - `game`: only on request. Xonotic plays its benchmark demo full screen at ultra settings (P1-10), see `game_load`.
//!
//! Each scenario writes its own `capture.json`, `capture.h264` and `load.json` into a subfolder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rcs_protocol::BUILD_ID;
use serde_json::{Value, json};
use tracing::{info, info_span, warn};

use crate::capture;
use crate::load::{self, LoadKind};
use crate::logging;
use crate::options::Options;

const REPORT_FILE: &str = "scenarios.json";
const CLIP_FILE: &str = "video-clip.mp4";
const CLIP_SECONDS: u32 = 20;
/// Time for the load to appear and settle before the capture starts.
const WARMUP: Duration = Duration::from_secs(3);
const GAME_WARMUP: Duration = Duration::from_secs(40);
/// The load runs this much longer than the capture, so it never ends inside the measured window.
const LOAD_TAIL_SECONDS: u32 = 5;
/// A scenario keeps up when the capture misses at most this share of the desktop presents. A present counts as
/// missed when it was merged into the next frame while the loop was busy encoding.
const MISSED_PRESENTS_LIMIT: f64 = 0.005;
/// The source runs at the display rate when it presents at least this share of the refresh rate.
const SOURCE_RATE_SHARE: f64 = 0.97;
/// A source presenting this much faster than the display is treated as uncapped.
const FASTER_SOURCE_SHARE: f64 = 1.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Static,
    Window,
    Video,
    Switch,
    Gpu,
    Game,
}

/// Run when `--scenarios` is not given. `switch` changes the display mode and runs only on request.
const ALL: [Scenario; 3] = [Scenario::Static, Scenario::Window, Scenario::Video];

impl Scenario {
    fn parse(name: &str) -> Option<Self> {
        ALL.into_iter()
            .chain([Self::Switch, Self::Gpu, Self::Game])
            .find(|s| s.name() == name)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Window => "window",
            Self::Video => "video",
            Self::Switch => "switch",
            Self::Gpu => "gpu",
            Self::Game => "game",
        }
    }

    /// Time for the load to appear and settle before the capture starts; the game loads its map first.
    fn warmup(self) -> Duration {
        match self {
            Self::Game => GAME_WARMUP,
            _ => WARMUP,
        }
    }

    /// Whether the scenario changes the screen at the display rate.
    fn moving(self) -> bool {
        self != Self::Static
    }
}

pub(crate) struct Tools {
    pub ffmpeg: PathBuf,
    pub ffplay: PathBuf,
}

pub(crate) fn find_tools(options: &Options) -> Result<Tools, String> {
    let dirs: Vec<PathBuf> = match &options.ffmpeg_dir {
        Some(dir) => vec![dir.clone()],
        None => std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default(),
    };
    let find = |exe: &str| dirs.iter().map(|d| d.join(exe)).find(|p| p.is_file());
    match (find("ffmpeg.exe"), find("ffplay.exe")) {
        (Some(ffmpeg), Some(ffplay)) => Ok(Tools { ffmpeg, ffplay }),
        _ => Err(
            "the video scenario needs ffmpeg.exe and ffplay.exe on PATH or in --ffmpeg-dir".into(),
        ),
    }
}

/// The synthetic clip for the video scenario, made once and kept in the output folder.
fn ensure_clip(tools: &Tools, out: &Path) -> Result<PathBuf, String> {
    let clip = out.join(CLIP_FILE);
    if clip.is_file() {
        return Ok(clip);
    }
    println!("Готовлю ролик {CLIP_SECONDS} с для сценария video...");
    let d = CLIP_SECONDS;
    let graph = format!(
        "gradients=s=1920x1080:r=60:n=6:speed=0.02:d={d}[g];testsrc2=s=960x540:r=60:d={d}[t];\
         [g][t]overlay=x='(W-w)/2+(W-w)/2*sin(t*1.3)':y='(H-h)/2+(H-h)/2*cos(t*0.9)':shortest=1,\
         noise=alls=6:allf=t,format=yuv420p[v]"
    );
    let partial = out.join("video-clip.partial.mp4");
    let status = Command::new(&tools.ffmpeg)
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
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-g", "60", "-an",
        ])
        .args(["-f", "mp4"])
        .arg(&partial)
        .status()
        .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg could not make the clip: {status}"));
    }
    fs::rename(&partial, &clip).map_err(|e| format!("cannot keep the clip: {e}"))?;
    Ok(clip)
}

fn start_load(
    scenario: Scenario,
    options: &Options,
    dir: &Path,
    clip: Option<&Path>,
    tools: Option<&Tools>,
) -> Result<Child, String> {
    let seconds = options.seconds + scenario.warmup().as_secs() as u32 + LOAD_TAIL_SECONDS;
    let mut command = match scenario {
        Scenario::Static | Scenario::Window | Scenario::Switch | Scenario::Gpu | Scenario::Game => {
            let kind = match scenario {
                Scenario::Static => LoadKind::Still,
                Scenario::Switch => LoadKind::Switch,
                Scenario::Gpu => LoadKind::Gpu,
                Scenario::Game => LoadKind::Game,
                _ => LoadKind::Window,
            };
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut c = Command::new(exe);
            c.args([
                "load",
                "--kind",
                kind.name(),
                "--seconds",
                &seconds.to_string(),
                "--out",
            ])
            .arg(dir);
            if let Some(game) = &options.game_exe {
                c.arg("--game-exe").arg(game);
            }
            c
        }
        Scenario::Video => {
            let (Some(tools), Some(clip)) = (tools, clip) else {
                return Err("the video scenario needs ffplay and the clip".into());
            };
            let mut c = Command::new(&tools.ffplay);
            // Without audio ffplay follows an external clock and drops frames; video sync keeps 60 fps.
            c.args([
                "-v",
                "error",
                "-fs",
                "-alwaysontop",
                "-noborder",
                "-an",
                "-loop",
                "0",
            ])
            .args(["-sync", "video", "-window_title", "rcs-bench video"])
            .arg(clip);
            c
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the {} load: {e}", scenario.name()))
}

/// Waits for `rcs-bench load` to finish on its own and write its report, then stops it. ffplay loops the clip and
/// is stopped at once.
fn stop_load(child: &mut Child, wait: bool) -> Option<i32> {
    let grace = if wait {
        u64::from(LOAD_TAIL_SECONDS) + 5
    } else {
        0
    };
    let deadline = Instant::now() + Duration::from_secs(grace);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn number(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn round(v: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (v * scale).round() / scale
}

/// The numbers the Gate asks for, taken from one capture report.
fn summarize(scenario: Scenario, report: &Value, load: Value, load_exit: Option<i32>) -> Value {
    let f = &report["frames"];
    let elapsed = number(&report["elapsed_s"]).max(1e-9);
    let captured = number(&f["captured"]);
    let presents = captured + number(&f["desktop_presents_merged"]);
    let missed = number(&f["merged_while_busy"]);
    let refresh = number(&report["display"]["refresh_hz"]);
    let presents_per_s = presents / elapsed;
    let missed_share = if presents > 0.0 {
        missed / presents
    } else {
        0.0
    };
    let failure = report["failure"].clone();
    // A source faster than the display (a game without vsync) is sampled at the stream rate, so merges are expected;
    // there the pipeline keeps up when it encodes at the stream rate.
    let target_fps = number(&report["options"]["fps"]).min(refresh.max(1.0));
    let source_faster = presents_per_s > refresh * FASTER_SOURCE_SHARE;
    let rate_ok = if source_faster {
        number(&f["encoded_fps"]) >= target_fps * SOURCE_RATE_SHARE
    } else {
        missed_share <= MISSED_PRESENTS_LIMIT
    };
    let kept_up = failure.is_null()
        && number(&f["lost_outputs"]) == 0.0
        && number(&f["pts_mismatch"]) == 0.0
        && rate_ok;
    let source_at_rate = !scenario.moving() || presents_per_s >= refresh * SOURCE_RATE_SHARE;
    let stage = |key: &str| report["stages"][key]["summary"].clone();
    json!({
        "scenario": scenario.name(),
        "failure": failure,
        "capture": report["capture"],
        "forced_idr": f["forced_idr"],
        "elapsed_s": report["elapsed_s"],
        "refresh_hz": refresh,
        "desktop_presents_per_s": round(presents_per_s, 2),
        "captured_per_s": round(captured / elapsed, 2),
        "encoded_per_s": f["encoded_fps"],
        "captured": f["captured"],
        "repeated": f["repeated"],
        "encoded": f["encoded"],
        "missed_presents": missed,
        "missed_presents_percent": round(missed_share * 100.0, 3),
        "merged_by_source": f["merged_while_waiting"],
        "merged_by_pacing": f["merged_by_pacing"],
        "late_frames": f["late_frames"],
        "lost_outputs": f["lost_outputs"],
        "idr": f["idr"],
        "bitrate_mbps": report["stream"]["bitrate_mbps"],
        "frame_bytes": report["stages"]["frame_bytes"],
        "largest_frame": report["stream"]["largest_frame"],
        "writer_stalls": report["stream"]["writer_stalls"],
        "frame_stays_in_video_memory": report["memory"]["frame_stays_in_video_memory"],
        "stages_ms": {
            "present_to_acquire": stage("present_to_acquire_ms"),
            "copy": stage("copy_ms"),
            "convert": stage("convert_ms"),
            "encode": stage("encode_ms"),
            "total": stage("total_ms"),
        },
        "source_at_display_rate": source_at_rate,
        "pipeline_kept_up": kept_up,
        "load": load,
        "load_exit_code": load_exit,
    })
}

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    logging::init(&options.out.join("rcs-bench.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "scenarios").entered();
    capture::set_dpi_awareness();

    let list: Vec<Scenario> = if options.scenarios.is_empty() {
        ALL.to_vec()
    } else {
        options
            .scenarios
            .iter()
            .map(|n| Scenario::parse(n).ok_or_else(|| format!("unknown scenario: {n}")))
            .collect::<Result<_, _>>()?
    };
    let tools = if list.contains(&Scenario::Video) {
        Some(find_tools(options)?)
    } else {
        None
    };
    let clip = match &tools {
        Some(t) => Some(ensure_clip(t, &options.out)?),
        None => None,
    };
    let total_s = list
        .iter()
        .map(|s| options.seconds + s.warmup().as_secs() as u32 + LOAD_TAIL_SECONDS)
        .sum::<u32>();
    println!(
        "Сценарии {}: по {} с, всего около {} мин. Окна нагрузки закрываются щелчком мыши.",
        list.iter().map(|s| s.name()).collect::<Vec<_>>().join(", "),
        options.seconds,
        total_s.div_ceil(60)
    );

    let mut results = Vec::with_capacity(list.len());
    let mut failed = Vec::new();
    for scenario in list {
        println!("\n== {} ==", scenario.name());
        let dir = options.out.join(scenario.name());
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let _ = fs::remove_file(dir.join(load::REPORT_FILE));
        info!(scenario = scenario.name(), "scenario started");
        let mut child = match start_load(scenario, options, &dir, clip.as_deref(), tools.as_ref()) {
            Ok(child) => child,
            Err(e) => {
                warn!(scenario = scenario.name(), error = %e, "load did not start");
                failed.push(scenario.name());
                results.push(json!({ "scenario": scenario.name(), "failure": e }));
                continue;
            }
        };
        std::thread::sleep(scenario.warmup());
        if let Ok(Some(status)) = child.try_wait() {
            let e = format!("the load stopped before the capture: {status}");
            failed.push(scenario.name());
            results.push(json!({ "scenario": scenario.name(), "failure": e }));
            continue;
        }
        let capture_options = Options {
            out: dir.clone(),
            ..options.clone()
        };
        let report = capture::measure(&capture_options);
        let exit = stop_load(&mut child, scenario != Scenario::Video);
        let load = read_json(&dir.join(load::REPORT_FILE));
        let summary = match report {
            Ok(report) => summarize(scenario, &report, load, exit),
            Err(e) => json!({ "scenario": scenario.name(), "failure": e, "load": load }),
        };
        if !summary["failure"].is_null() || summary["pipeline_kept_up"] == false {
            failed.push(scenario.name());
        }
        info!(scenario = scenario.name(), summary = %summary, "scenario finished");
        results.push(summary);
    }

    let report = json!({
        "tool": "rcs-bench scenarios",
        "build": BUILD_ID,
        "seconds": options.seconds,
        "width": options.encode.width,
        "height": options.encode.height,
        "fps": options.encode.fps,
        "bitrate": options.encode.bitrate,
        "missed_presents_limit_percent": MISSED_PRESENTS_LIMIT * 100.0,
        "scenarios": results,
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    println!("\nИтог:");
    for s in report["scenarios"].as_array().into_iter().flatten() {
        if !s["failure"].is_null() {
            println!("  {}: ошибка: {}", s["scenario"], s["failure"]);
            continue;
        }
        let total = &s["stages_ms"]["total"];
        println!(
            "  {}: presents {}/с, закодировано {}/с, пропущено {} ({}%), захват→пакет p95 {} мс, p99 {} мс, {} Мбит/с, успевает: {}",
            s["scenario"],
            s["desktop_presents_per_s"],
            s["encoded_per_s"],
            s["missed_presents"],
            s["missed_presents_percent"],
            total["p95"],
            total["p99"],
            s["bitrate_mbps"],
            s["pipeline_kept_up"]
        );
    }
    println!("Отчёт: {}", path.display());
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("scenarios with problems: {}", failed.join(", ")))
    }
}
