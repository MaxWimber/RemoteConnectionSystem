//! `rcs-bench probe`: adapters, displays, Desktop Duplication, Media Foundation encoders and the AMF runtime.
//! AMF runs in a child process, so a crash inside the driver still leaves the rest of the report.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcs_capture::dxgi::{self, Adapter, DuplicationReport};
use rcs_encode::{amf, mf};
use rcs_protocol::{BUILD_ID, PROTOCOL_VERSION};
use serde_json::{Value, json};
use tracing::{info, info_span, warn};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::logging;
use crate::options::Options;
use crate::stats::summarize;

const REPORT_FILE: &str = "probe.json";
const AMF_FILE: &str = "probe-amf.json";
const AMD_VENDOR_ID: u32 = 0x1002;
const DUPLICATION_TIME: Duration = Duration::from_secs(2);
const AMF_TIMEOUT: Duration = Duration::from_secs(300);
const USAGE_NAMES: [&str; 6] = [
    "transcoding",
    "ultra_low_latency",
    "low_latency",
    "webcam",
    "high_quality",
    "low_latency_high_quality",
];

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    logging::init(&options.out.join("rcs-bench.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "probe").entered();
    info!(?options, "probe started");

    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut report = json!({
        "tool": "rcs-bench probe",
        "build": BUILD_ID,
        "protocol": PROTOCOL_VERSION,
        "started_unix": started,
        "options": {
            "width": options.encode.width,
            "height": options.encode.height,
            "fps": options.encode.fps,
            "frames": options.encode.frames,
            "bitrate": options.encode.bitrate,
        },
    });
    let path = options.out.join(REPORT_FILE);

    if let Err(error) = dxgi::set_dpi_awareness() {
        report["dpi_awareness_error"] = json!(dxgi::describe(&error));
    }
    // SAFETY: called once on this thread before any COM or Media Foundation use.
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        report["com_error"] = json!(dxgi::describe(&error));
    }

    println!("Видеоадаптеры и захват экрана...");
    let adapters = match dxgi::enumerate_adapters() {
        Ok(adapters) => adapters,
        Err(error) => {
            report["adapters_error"] = json!(dxgi::describe(&error));
            Vec::new()
        }
    };
    report["adapters"] = Value::Array(adapters.iter().map(adapter_report).collect());
    save(&path, &report)?;

    println!("Кодировщики Media Foundation...");
    report["media_foundation"] = media_foundation_report();
    save(&path, &report)?;

    let target = adapters
        .iter()
        .find(|a| a.info.vendor_id == AMD_VENDOR_ID && !a.info.software)
        .or_else(|| adapters.iter().find(|a| !a.info.software));
    report["amf"] = match target {
        Some(adapter) => {
            println!("AMF на адаптере {}, около минуты...", adapter.info.name);
            amf_report(options, adapter)
        }
        None => json!({ "skipped": "no hardware adapter" }),
    };
    save(&path, &report)?;

    print_summary(&report);
    println!("Отчёт: {}", path.display());
    info!("probe finished");
    Ok(())
}

/// Child process: one adapter, one device, the AMF shim.
pub fn run_amf(options: &Options) -> Result<(), String> {
    logging::init(&options.out.join("rcs-bench-amf.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "amf-probe").entered();
    if let Err(error) = dxgi::set_dpi_awareness() {
        warn!(error = %dxgi::describe(&error), "dpi awareness");
    }
    let index = options.adapter.ok_or("--adapter is required")?;
    let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
    let adapter = adapters
        .iter()
        .find(|a| a.info.index == index)
        .ok_or(format!("no adapter {index}"))?;
    let (device, _) = dxgi::create_device(&adapter.adapter).map_err(|e| dxgi::describe(&e))?;
    info!(adapter = %adapter.info.name, "amf probe started");
    let text = amf::probe(&device, &options.encode).ok_or("the AMF shim returned no report")?;
    fs::write(options.out.join(AMF_FILE), text).map_err(|e| e.to_string())?;
    info!("amf probe finished");
    Ok(())
}

fn save(path: &Path, report: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn adapter_report(adapter: &Adapter) -> Value {
    let info = &adapter.info;
    let mut value = json!({
        "index": info.index,
        "name": info.name,
        "vendor_id": format!("{:#06x}", info.vendor_id),
        "device_id": format!("{:#06x}", info.device_id),
        "revision": info.revision,
        "dedicated_video_memory_mib": info.dedicated_video_memory >> 20,
        "shared_system_memory_mib": info.shared_system_memory >> 20,
        "luid": format!("{:#018x}", info.luid),
        "software": info.software,
        "driver_version": info.driver_version,
        "outputs": info.outputs.iter().map(|o| json!({
            "index": o.index,
            "device_name": o.device_name,
            "desktop": o.desktop,
            "attached": o.attached,
            "rotation": o.rotation,
            "bits_per_color": o.bits_per_color,
            "color_space": o.color_space,
            "mode": o.mode.map(|(w, h, hz)| format!("{w}x{h}@{hz}")),
        })).collect::<Vec<_>>(),
    });
    if info.software || info.outputs.is_empty() {
        return value;
    }
    match dxgi::create_device(&adapter.adapter) {
        Ok((device, level)) => {
            value["feature_level"] = json!(format!("{:#x}", level.0));
            let reports: Vec<Value> = info
                .outputs
                .iter()
                .map(|o| {
                    duplication_report(&dxgi::probe_duplication(
                        &adapter.adapter,
                        &device,
                        o.index,
                        DUPLICATION_TIME,
                    ))
                })
                .collect();
            value["duplication"] = Value::Array(reports);
        }
        Err(error) => value["device_error"] = json!(dxgi::describe(&error)),
    }
    value
}

fn format_name(format: i32) -> &'static str {
    match format {
        87 => "B8G8R8A8_UNORM",
        24 => "R10G10B10A2_UNORM",
        10 => "R16G16B16A16_FLOAT",
        28 => "R8G8B8A8_UNORM",
        _ => "other",
    }
}

fn duplication_report(r: &DuplicationReport) -> Value {
    if r.error.is_some() {
        warn!(output = r.output_index, error = ?r.error, "duplication failed");
    }
    json!({
        "output": r.output_index,
        "error": r.error,
        "width": r.width,
        "height": r.height,
        "refresh": format!("{}/{}", r.refresh.0, r.refresh.1),
        "format": r.format,
        "format_name": format_name(r.format),
        "rotation": r.rotation,
        "desktop_in_system_memory": r.desktop_in_system_memory,
        "frames": r.frames,
        "timeouts": r.timeouts,
        "access_lost": r.access_lost,
        "acquire_ms": summarize(&r.acquire_ms),
        "texture_format": r.texture_format,
        "texture_bind_flags": r.texture_bind_flags,
        "texture_misc_flags": r.texture_misc_flags,
        "elapsed_ms": r.elapsed_ms.round(),
    })
}

fn media_foundation_report() -> Value {
    let runtime = match mf::Runtime::start() {
        Ok(runtime) => runtime,
        Err(error) => return json!({ "error": dxgi::describe(&error) }),
    };
    match mf::h264_encoders(&runtime) {
        Ok(list) => json!({
            "h264_encoders": list.iter().map(|e| json!({
                "name": e.name,
                "hardware": e.hardware,
                "vendor_id": e.vendor_id,
                "adapter_luid": e.adapter_luid.map(|l| format!("{l:#018x}")),
                "transform_flags": e.transform_flags,
            })).collect::<Vec<_>>(),
        }),
        Err(error) => json!({ "error": dxgi::describe(&error) }),
    }
}

fn amf_report(options: &Options, adapter: &Adapter) -> Value {
    let file = options.out.join(AMF_FILE);
    let _ = fs::remove_file(&file);
    let mut result = json!({ "adapter": adapter.info.index, "adapter_name": adapter.info.name });
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            result["error"] = json!(error.to_string());
            return result;
        }
    };
    let e = &options.encode;
    let spawned = Command::new(exe)
        .arg("amf-probe")
        .arg("--out")
        .arg(&options.out)
        .args(["--adapter", &adapter.info.index.to_string()])
        .args([
            "--width",
            &e.width.to_string(),
            "--height",
            &e.height.to_string(),
        ])
        .args([
            "--fps",
            &e.fps.to_string(),
            "--frames",
            &e.frames.to_string(),
        ])
        .args(["--bitrate-mbps", &(e.bitrate / 1_000_000).to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            result["error"] = json!(error.to_string());
            return result;
        }
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > AMF_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                result["error"] = json!(error.to_string());
                break None;
            }
        }
    };
    result["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    result["timed_out"] = json!(status.is_none() && started.elapsed() > AMF_TIMEOUT);
    // A crash shows up as an NTSTATUS such as 0xc0000005.
    result["exit_code"] = json!(
        status
            .and_then(|s| s.code())
            .map(|c| format!("{:#010x}", c as u32))
    );

    match fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(mut probe) => {
                add_run_summaries(&mut probe, options.encode.fps);
                result["report"] = probe;
                let _ = fs::remove_file(&file);
            }
            Err(error) => result["parse_error"] = json!(error.to_string()),
        },
        Err(error) => result["read_error"] = json!(error.to_string()),
    }
    result
}

fn series(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

fn add_run_summaries(probe: &mut Value, fps: i32) {
    let Some(runs) = probe.get_mut("encode_runs").and_then(Value::as_array_mut) else {
        return;
    };
    for run in runs {
        let usage = run["usage"].as_i64().unwrap_or(-1);
        run["usage_name"] = json!(
            USAGE_NAMES
                .get(usage as usize)
                .copied()
                .unwrap_or("unknown")
        );
        run["convert_summary"] = summarize(&series(&run["convert_ms"]));
        run["encode_summary"] = summarize(&series(&run["encode_ms"]));
        let outputs = run["outputs"].as_f64().unwrap_or(0.0);
        let bytes = run["bytes"].as_f64().unwrap_or(0.0);
        if outputs > 0.0 {
            run["bitrate_mbps_at_fps"] =
                json!((bytes * 8.0 * f64::from(fps) / outputs / 1e4).round() / 100.0);
        }
    }
}

fn result_text(value: &Value) -> String {
    match value["code"].as_i64() {
        Some(0) => "OK".to_string(),
        Some(code) => format!("{code} {}", value["text"].as_str().unwrap_or("")),
        None => "-".to_string(),
    }
}

fn print_summary(report: &Value) {
    println!();
    for adapter in report["adapters"].as_array().into_iter().flatten() {
        println!(
            "Адаптер {}: {}, драйвер {}",
            adapter["index"],
            adapter["name"].as_str().unwrap_or("?"),
            adapter["driver_version"].as_str().unwrap_or("?")
        );
        for d in adapter["duplication"].as_array().into_iter().flatten() {
            match d["error"].as_str() {
                Some(error) => println!("  Захват выхода {}: ошибка {error}", d["output"]),
                None => println!(
                    "  Захват выхода {}: {}x{} @ {}, кадров {}",
                    d["output"],
                    d["width"],
                    d["height"],
                    d["refresh"].as_str().unwrap_or("?"),
                    d["frames"]
                ),
            }
        }
    }
    let hardware: Vec<&str> = report["media_foundation"]["h264_encoders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["hardware"].as_bool() == Some(true))
        .filter_map(|e| e["name"].as_str())
        .collect();
    println!(
        "Аппаратные кодировщики H.264 в Media Foundation: {}",
        if hardware.is_empty() {
            "нет".to_string()
        } else {
            hardware.join(", ")
        }
    );

    let amf = &report["amf"];
    let probe = &amf["report"];
    if probe.is_null() {
        println!("AMF: отчёт не получен, код завершения {}", amf["exit_code"]);
        return;
    }
    if probe["dll"]["loaded"].as_bool() != Some(true) {
        println!("AMF: amfrt64.dll не найдена");
        return;
    }
    println!(
        "AMF: среда {}, инициализация {}",
        probe["runtime_version"].as_str().unwrap_or("?"),
        result_text(&probe["init"])
    );
    for run in probe["encode_runs"].as_array().into_iter().flatten() {
        let encode = &run["encode_summary"];
        let timing = if encode.is_null() {
            String::new()
        } else {
            format!(
                ", кодирование p50 {} мс, p95 {} мс",
                encode["p50"], encode["p95"]
            )
        };
        println!(
            "  {} {}: {}{}",
            run["usage_name"].as_str().unwrap_or("?"),
            if run["low_latency_mode_set"].as_bool() == Some(true) {
                "+LowLatencyInternal"
            } else {
                ""
            },
            result_text(&run["init"]),
            timing
        );
    }
}
