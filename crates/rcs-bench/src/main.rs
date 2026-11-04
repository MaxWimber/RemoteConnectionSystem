//! Capture and encoder measurements.

mod logging;
mod options;
mod probe;
mod stats;
mod ull;

use std::process::ExitCode;

const USAGE: &str = "\
Usage:
  rcs-bench                 print the build
  rcs-bench probe --out DIR [--frames N] [--width W] [--height H] [--fps F] [--bitrate-mbps B]
  rcs-bench ull --out DIR [same options]       which settings Ultra Low Latency accepts
  rcs-bench amf-probe|amf-ull --out DIR --adapter N [same options]   child processes of probe and ull";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        println!("rcs-bench {}", rcs_protocol::BUILD_ID);
        return ExitCode::SUCCESS;
    };
    let options = match options::parse(&args[1..]) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let outcome = match command.as_str() {
        "probe" => probe::run(&options),
        "ull" => ull::run(&options),
        "amf-probe" => probe::run_amf(&options, probe::AmfProbe::Full),
        "amf-ull" => probe::run_amf(&options, probe::AmfProbe::UltraLowLatency),
        other => Err(format!("Unknown command: {other}\n{USAGE}")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!(%message, "command failed");
            eprintln!("Ошибка: {message}");
            ExitCode::FAILURE
        }
    }
}
