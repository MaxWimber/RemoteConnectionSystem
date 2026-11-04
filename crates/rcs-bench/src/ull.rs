//! `rcs-bench ull`: which encoder settings Ultra Low Latency accepts on this runtime, see Q-01.

use std::fs;

use rcs_capture::dxgi;
use rcs_protocol::BUILD_ID;
use serde_json::{Value, json};
use tracing::{info, info_span};

use crate::logging;
use crate::options::Options;
use crate::probe::{self, AmfProbe};

const REPORT_FILE: &str = "ull.json";
const RUN_LISTS: [&str; 6] = [
    "baseline",
    "single",
    "greedy",
    "final",
    "sizes",
    "workarounds",
];

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    logging::init(&options.out.join("rcs-bench.jsonl"))?;
    let _span = info_span!("rcs", build = BUILD_ID, tool = "ull").entered();
    info!(?options, "ull probe started");
    if let Err(error) = dxgi::set_dpi_awareness() {
        tracing::warn!(error = %dxgi::describe(&error), "dpi awareness");
    }

    let adapters = dxgi::enumerate_adapters().map_err(|e| dxgi::describe(&e))?;
    let adapter = probe::encoder_adapter(&adapters).ok_or("no hardware adapter")?;
    println!("Проба Ultra Low Latency на {}...", adapter.info.name);
    let mut amf = probe::run_amf_child(options, adapter, AmfProbe::UltraLowLatency);
    if let Some(report) = amf.get_mut("report") {
        for key in RUN_LISTS {
            probe::add_run_summaries(report, key, options.encode.fps);
        }
    }
    let report = json!({
        "tool": "rcs-bench ull",
        "build": BUILD_ID,
        "options": {
            "width": options.encode.width,
            "height": options.encode.height,
            "fps": options.encode.fps,
            "frames": options.encode.frames,
            "bitrate": options.encode.bitrate,
        },
        "amf": amf,
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    print_summary(&report["amf"]);
    println!("Отчёт: {}", path.display());
    info!("ull probe finished");
    Ok(())
}

fn props(run: &Value) -> String {
    let list: Vec<&str> = run["props"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if list.is_empty() {
        "только usage".to_string()
    } else {
        list.join(", ")
    }
}

fn print_run(run: &Value) {
    let encode = &run["encode_summary"];
    let timing = if encode.is_null() {
        String::new()
    } else {
        format!(
            ", кодирование p50 {} мс, p95 {} мс, {} Мбит/с",
            encode["p50"], encode["p95"], run["bitrate_mbps_at_fps"]
        )
    };
    println!(
        "  {} {}x{} [{}]: {}{}",
        run["usage_name"].as_str().unwrap_or("?"),
        run["width"],
        run["height"],
        props(run),
        probe::result_text(&run["init"]),
        timing
    );
}

fn print_summary(amf: &Value) {
    let report = &amf["report"];
    if report.is_null() {
        println!("AMF: отчёт не получен, код завершения {}", amf["exit_code"]);
        return;
    }
    if let Some(error) = report["error"].as_str() {
        println!("AMF: ошибка {error}");
        return;
    }
    for key in RUN_LISTS {
        println!("{key}:");
        for run in report[key].as_array().into_iter().flatten() {
            print_run(run);
        }
    }
    println!(
        "Принятые свойства: {}",
        props(&json!({ "props": report["accepted"] }))
    );
}
