//! `%ProgramData%\RCS\logs\audit.log`: sign-ins, blocks and stream sessions, one JSON object per line.
//!
//! Lines go through a channel to a writer thread, so request handlers never wait for the disk. The file rolls
//! over at 10 MiB and five old files are kept. Nothing secret is written: no passwords, codes, cookies or SDP.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::util::rfc3339;

const ROLL_BYTES: u64 = 10 * 1024 * 1024;
const KEEP: u32 = 5;

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    base.join("RCS").join("logs").join("audit.log")
}

#[derive(Clone)]
pub struct Audit {
    tx: mpsc::Sender<String>,
}

impl Audit {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let file = open(&path)?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("rcs-audit".into())
            .spawn(move || writer(path, file, rx))
            .map_err(|e| e.to_string())?;
        Ok(Self { tx })
    }

    /// Records `event`; `fields` must hold no secrets.
    pub fn event(&self, event: &str, ip: Option<IpAddr>, fields: Value) {
        let mut line = Map::new();
        line.insert("time".into(), json!(rfc3339(SystemTime::now())));
        line.insert("event".into(), json!(event));
        if let Some(ip) = ip {
            line.insert("ip".into(), json!(ip.to_string()));
        }
        if let Value::Object(extra) = fields {
            line.extend(extra);
        }
        tracing::info!(target: "audit", event, ip = ?ip, "audit");
        let _ = self.tx.send(Value::Object(line).to_string());
    }
}

fn open(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn roll(path: &Path) {
    let name = |n: u32| PathBuf::from(format!("{}.{n}", path.display()));
    let _ = std::fs::remove_file(name(KEEP));
    for n in (1..KEEP).rev() {
        let _ = std::fs::rename(name(n), name(n + 1));
    }
    let _ = std::fs::rename(path, name(1));
}

fn writer(path: PathBuf, mut file: File, rx: mpsc::Receiver<String>) {
    for line in rx {
        if file.metadata().is_ok_and(|m| m.len() >= ROLL_BYTES) {
            drop(file);
            roll(&path);
            file = match open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(error = %e, "audit log unavailable");
                    return;
                }
            };
        }
        if let Err(e) = writeln!(file, "{line}").and_then(|()| file.flush()) {
            tracing::error!(error = %e, "audit write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_json_and_roll_over() {
        let dir = crate::secrets::tests::temp_dir("audit");
        let path = dir.join("audit.log");
        std::fs::write(&path, vec![b'x'; ROLL_BYTES as usize]).unwrap();
        let audit = Audit::open(path.clone()).unwrap();
        audit.event(
            "login_failed",
            Some("192.0.2.1".parse().unwrap()),
            json!({ "step": "password" }),
        );
        drop(audit);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let text = loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.starts_with('{') || std::time::Instant::now() > deadline {
                break text;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let line: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["event"], "login_failed");
        assert_eq!(line["ip"], "192.0.2.1");
        assert_eq!(line["step"], "password");
        assert!(PathBuf::from(format!("{}.1", path.display())).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
