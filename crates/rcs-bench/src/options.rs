//! Command line options shared by all commands.

use std::path::PathBuf;

use rcs_encode::amf::ProbeOptions;

#[derive(Debug, Clone)]
pub struct Options {
    pub out: PathBuf,
    pub adapter: Option<u32>,
    pub encode: ProbeOptions,
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
    fn rejects_bad_input() {
        assert!(parse(&args(&["--frames"])).is_err());
        assert!(parse(&args(&["--frames", "abc"])).is_err());
        assert!(parse(&args(&["--width", "1921"])).is_err());
        assert!(parse(&args(&["--unknown", "1"])).is_err());
    }
}
