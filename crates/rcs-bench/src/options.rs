//! Command line options shared by all commands.

use std::path::PathBuf;

use rcs_encode::amf::ProbeOptions;

#[derive(Debug, Clone)]
pub struct Options {
    pub out: PathBuf,
    pub adapter: Option<u32>,
    pub encode: ProbeOptions,
    /// Capture: run time, display output, minimum frame rate on a static screen, VBV size.
    pub seconds: u32,
    pub display: u32,
    pub repeat_fps: u32,
    pub vbv_kbit: Option<i64>,
    pub query_timeout_ms: Option<i32>,
    /// `cbr` or `lcvbr` (latency constrained VBR); the default of `EncoderConfig` when absent.
    pub rate_control: Option<i32>,
    /// Capture and scenarios: `auto`, `amf` or `mf`, as `media.encoder`.
    pub backend: rcs_encode::Backend,
    /// Load: `still` or `window`.
    pub kind: Option<String>,
    /// Scenarios: which ones to run, all when empty.
    pub scenarios: Vec<String>,
    /// Folder with ffmpeg.exe and ffplay.exe when they are not on PATH.
    pub ffmpeg_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            out: PathBuf::from("out"),
            adapter: None,
            encode: ProbeOptions {
                width: 1920,
                height: 1080,
                fps: 60,
                frames: 120,
                bitrate: 30_000_000,
            },
            seconds: 10,
            display: 0,
            repeat_fps: 15,
            vbv_kbit: None,
            query_timeout_ms: None,
            rate_control: None,
            backend: rcs_encode::Backend::Auto,
            kind: None,
            scenarios: Vec::new(),
            ffmpeg_dir: None,
        }
    }
}

pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<i64>()
                .map_err(|_| format!("{flag}: not a number: {value}"))
        };
        match flag.as_str() {
            "--out" => options.out = PathBuf::from(value),
            "--adapter" => options.adapter = Some(positive(flag, number()?)? as u32),
            "--frames" => options.encode.frames = positive(flag, number()?)? as i32,
            "--width" => options.encode.width = even(flag, number()?)? as i32,
            "--height" => options.encode.height = even(flag, number()?)? as i32,
            "--fps" => options.encode.fps = positive(flag, number()?)? as i32,
            "--bitrate-mbps" => options.encode.bitrate = positive(flag, number()?)? * 1_000_000,
            "--seconds" => options.seconds = positive(flag, number()?)? as u32,
            "--display" => options.display = positive(flag, number()?)? as u32,
            "--repeat-fps" => options.repeat_fps = positive(flag, number()?)? as u32,
            "--vbv-kbit" => options.vbv_kbit = Some(positive(flag, number()?)?),
            "--query-timeout-ms" => {
                options.query_timeout_ms = Some(positive(flag, number()?)?.min(1000) as i32)
            }
            "--rate-control" => {
                options.rate_control = Some(match value.as_str() {
                    "cbr" => rcs_encode::amf::RATE_CONTROL_CBR,
                    "lcvbr" => rcs_encode::amf::RATE_CONTROL_LATENCY_CONSTRAINED_VBR,
                    _ => return Err(format!("{flag}: cbr or lcvbr")),
                })
            }
            "--encoder" => {
                options.backend = rcs_encode::Backend::parse(value)
                    .ok_or_else(|| format!("{flag}: auto, amf or mf"))?
            }
            "--kind" => options.kind = Some(value.clone()),
            "--scenarios" => {
                options.scenarios = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--ffmpeg-dir" => options.ffmpeg_dir = Some(PathBuf::from(value)),
            _ => return Err(format!("Unknown option: {flag}")),
        }
    }
    Ok(options)
}

fn positive(flag: &str, value: i64) -> Result<i64, String> {
    if (0..=100_000).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{flag}: out of range: {value}"))
    }
}

fn even(flag: &str, value: i64) -> Result<i64, String> {
    // The synthetic pattern and NV12 need an even size, and the noise block needs room to move.
    if value >= 64 && value % 2 == 0 && value <= 8192 {
        Ok(value)
    } else {
        Err(format!("{flag}: need an even size 64..8192"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_match_the_target_mode() {
        let options = parse(&[]).unwrap();
        assert_eq!(
            (
                options.encode.width,
                options.encode.height,
                options.encode.fps
            ),
            (1920, 1080, 60)
        );
        assert_eq!(options.encode.bitrate, 30_000_000);
    }

    #[test]
    fn reads_all_flags() {
        let options = parse(&args(&[
            "--out",
            "x",
            "--adapter",
            "1",
            "--frames",
            "10",
            "--bitrate-mbps",
            "8",
        ]))
        .unwrap();
        assert_eq!(options.out, PathBuf::from("x"));
        assert_eq!(options.adapter, Some(1));
        assert_eq!(options.encode.frames, 10);
        assert_eq!(options.encode.bitrate, 8_000_000);
    }

    #[test]
    fn reads_scenario_list() {
        let options = parse(&args(&[
            "--scenarios",
            "static, video,,window",
            "--kind",
            "still",
        ]))
        .unwrap();
        assert_eq!(options.scenarios, ["static", "video", "window"]);
        assert_eq!(options.kind.as_deref(), Some("still"));
    }

    #[test]
    fn reads_encoder_and_rate_control() {
        let options = parse(&args(&["--encoder", "mf", "--rate-control", "cbr"])).unwrap();
        assert_eq!(options.backend, rcs_encode::Backend::Mf);
        assert_eq!(
            options.rate_control,
            Some(rcs_encode::amf::RATE_CONTROL_CBR)
        );
        assert_eq!(parse(&[]).unwrap().backend, rcs_encode::Backend::Auto);
        assert!(parse(&args(&["--encoder", "nvenc"])).is_err());
        assert!(parse(&args(&["--rate-control", "vbr"])).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&args(&["--frames"])).is_err());
        assert!(parse(&args(&["--frames", "abc"])).is_err());
        assert!(parse(&args(&["--width", "1921"])).is_err());
        assert!(parse(&args(&["--unknown", "1"])).is_err());
    }
}
