//! JSON lines log next to the reports, plus a panic hook that records the backtrace.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use tracing::Level;

pub fn init(path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_max_level(Level::DEBUG)
        .with_writer(Mutex::new(file))
        .try_init()
        .map_err(|e| format!("cannot start logging: {e}"))?;
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, %backtrace, "panic");
        eprintln!("{info}");
    }));
    Ok(())
}
