//! Capture and encoder measurements.

mod avsync;
mod capture;
mod game_load;
mod gpu_load;
mod load;
mod logging;
mod matrix;
mod netsim;
mod options;
mod probe;
mod scenarios;
mod stats;
mod ull;

use std::process::ExitCode;

const USAGE: &str = "\
Usage:
  rcs-bench                 print the build
  rcs-bench probe --out DIR [--frames N] [--width W] [--height H] [--fps F] [--bitrate-mbps B]
  rcs-bench ull --out DIR [same options]       which settings Ultra Low Latency accepts
  rcs-bench capture --out DIR [--seconds S] [--display N] [--repeat-fps F] [--vbv-kbit K] [same options]
  rcs-bench scenarios --out DIR [--seconds 300] [--scenarios static,window,video] [--ffmpeg-dir D] [capture options]
  rcs-bench load --out DIR --kind still|window|switch|gpu|game --seconds S [--game-exe X]   screen load, started by scenarios
  rcs-bench avsync --out DIR --seconds S   a square flashes with a click once a second, for «Замер звука» (P9-1)
  rcs-bench netsim --out DIR --listen ADDR --to ADDR --seconds S [--link S:MBPS,...] [--link-queue-ms MS]
                   [--link-loss PERCENT] [--outage EVERY_MS:BURST_MS]   UDP go-between that spoils the way back (P9-4)
  rcs-bench matrix --out DIR [--seconds 10] [--scenarios baseline,vbv_1_frame,...] [--ffmpeg-dir D]
                            AMF settings against fixed clips: timings, frame sizes, SSIM and PSNR
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
        "capture" => capture::run(&options),
        "scenarios" => scenarios::run(&options),
        "load" => load::run(&options),
        "avsync" => avsync::run(&options),
        "netsim" => netsim::run(&options),
        "matrix" => matrix::run(&options),
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
