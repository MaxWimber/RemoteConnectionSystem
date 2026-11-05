//! `rcs-bench load --kind game`: a real game as the screen load for P1-10.
//!
//! Xonotic (GPL, installed with winget as a portable zip) plays its demo `the-big-keybench`, a recorded match with
//! bots, in a loop at real time, full screen at 1920x1080 with the `effects-ultra` preset, no vsync and no frame cap:
//! the engine renders as fast as the GPU allows, like a game without vsync. `startdemos` keeps only short names, so
//! the demo is unpacked once from the data archive as `demos/kb.dem`. Should the game quit, it is started again.
//! Its settings live in the output folder, not in the user's profile. The result goes to `load.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::load::REPORT_FILE;
use crate::options::Options;

/// The winget package folder of Xonotic, under %LOCALAPPDATA%\Microsoft\WinGet\Packages.
fn find_game() -> Option<PathBuf> {
    let packages = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Microsoft")
        .join("WinGet")
        .join("Packages");
    fs::read_dir(packages)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("Xonotic.Xonotic")
        })
        .map(|e| e.path().join("Xonotic").join("xonotic.exe"))
        .find(|p| p.is_file())
}

/// The demo under its short name in the game's user folder.
const DEMO: &str = "demos/kb";
const DEMO_IN_ARCHIVE: &str = "demos/the-big-keybench.dem";

/// Unpacks the demo from the data archive with the `tar` of Windows, which reads zip files.
fn unpack_demo(exe: &Path, user_dir: &Path) -> Result<(), String> {
    let target = user_dir.join("data").join("demos").join("kb.dem");
    if target.is_file() {
        return Ok(());
    }
    let data = exe.parent().ok_or("the game has no folder")?.join("data");
    let archive = fs::read_dir(&data)
        .map_err(|e| format!("cannot read {}: {e}", data.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.starts_with("xonotic-") && name.ends_with("-data.pk3")
        })
        .ok_or("no data archive of the game")?;
    let scratch = user_dir.join("unpack");
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&scratch)
        .arg(DEMO_IN_ARCHIVE)
        .status()
        .map_err(|e| format!("cannot start tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar could not unpack the demo: {status}"));
    }
    fs::create_dir_all(target.parent().ok_or("bad demo path")?).map_err(|e| e.to_string())?;
    fs::rename(scratch.join(DEMO_IN_ARCHIVE), &target)
        .map_err(|e| format!("cannot keep the demo: {e}"))?;
    let _ = fs::remove_dir_all(&scratch);
    Ok(())
}

fn start(exe: &Path, user_dir: &Path) -> Result<Child, String> {
    let game_dir = exe.parent().ok_or("the game has no folder")?;
    Command::new(exe)
        .current_dir(game_dir)
        .arg("-basedir")
        .arg(game_dir)
        .arg("-userdir")
        .arg(user_dir)
        .arg("-nosound")
        .args(["+exec", "effects-ultra.cfg"])
        .args([
            "+vid_fullscreen",
            "1",
            "+vid_width",
            "1920",
            "+vid_height",
            "1080",
        ])
        .args([
            "+vid_vsync",
            "0",
            "+cl_maxfps",
            "0",
            "+log_file",
            "xonotic.log",
            "+cl_allow_uid2name",
            "0",
        ])
        .args(["+startdemos", DEMO, "+demos"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))
}

pub fn run(options: &Options) -> Result<(), String> {
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    let exe = match &options.game_exe {
        Some(exe) => exe.clone(),
        None => find_game().ok_or("Xonotic is not installed, see --game-exe")?,
    };
    let user_dir = std::path::absolute(options.out.join("game-user")).map_err(|e| e.to_string())?;
    fs::create_dir_all(&user_dir).map_err(|e| format!("cannot create the game folder: {e}"))?;
    unpack_demo(&exe, &user_dir)?;

    let duration = Duration::from_secs(u64::from(options.seconds));
    let started = Instant::now();
    let mut runs = Vec::new();
    let mut child = start(&exe, &user_dir)?;
    let mut run_started = Instant::now();
    while started.elapsed() < duration {
        match child.try_wait() {
            Ok(Some(status)) => {
                runs.push(json!({
                    "seconds": (run_started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                    "exit_code": status.code(),
                }));
                child = start(&exe, &user_dir)?;
                run_started = Instant::now();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(format!("cannot watch the game: {e}")),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    runs.push(json!({
        "seconds": (run_started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "exit_code": null,
        "stopped": true,
    }));

    let report = json!({
        "kind": "game",
        "game": exe.display().to_string(),
        "demo": "demos/the-big-keybench, looped",
        "preset": "effects-ultra",
        "seconds": (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
        "starts": runs.len(),
        "runs": runs,
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}
