//! What the agent holds: memory, handles, GDI and USER objects, video memory. Sampled on its own thread and written
//! to the log, so a long run shows a leak as a line that keeps climbing (P8-7).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::info;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
    IDXGIFactory1,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
};
use windows::core::Interface;

const AMD_VENDOR_ID: u32 = 0x1002;

/// One look at the process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sample {
    pub working_set: u64,
    /// Committed memory only this process uses: the number that grows with a leak.
    pub private_bytes: u64,
    pub handles: u32,
    pub gdi_objects: u32,
    pub user_objects: u32,
    /// Video memory this process uses on the AMD card, local and shared with the system; none without the card.
    pub gpu_local: Option<u64>,
    pub gpu_non_local: Option<u64>,
}

pub fn sample() -> Sample {
    let mut s = Sample::default();
    // SAFETY: the pseudo handle of this process needs no closing.
    let process = unsafe { GetCurrentProcess() };
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the counters struct lives on the stack and `cb` gives its size; the EX layout starts with the basic one.
    if unsafe { GetProcessMemoryInfo(process, (&raw mut memory).cast(), memory.cb) }.is_ok() {
        s.working_set = memory.WorkingSetSize as u64;
        s.private_bytes = memory.PrivateUsage as u64;
    }
    let mut handles = 0u32;
    // SAFETY: the out pointer is a live local.
    if unsafe { GetProcessHandleCount(process, &mut handles) }.is_ok() {
        s.handles = handles;
    }
    // SAFETY: plain queries of this process.
    unsafe {
        s.gdi_objects = GetGuiResources(process, GR_GDIOBJECTS);
        s.user_objects = GetGuiResources(process, GR_USEROBJECTS);
    }
    if let Some((local, non_local)) = video_memory() {
        s.gpu_local = Some(local);
        s.gpu_non_local = Some(non_local);
    }
    s
}

/// This process's use of the AMD card's memory, from `IDXGIAdapter3::QueryVideoMemoryInfo`.
fn video_memory() -> Option<(u64, u64)> {
    // SAFETY: no arguments besides the requested interface.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    for index in 0.. {
        // SAFETY: the factory is valid; an error marks the end of the list.
        let adapter = unsafe { factory.EnumAdapters1(index) }.ok()?;
        // SAFETY: the adapter is valid.
        let desc = unsafe { adapter.GetDesc1() }.ok()?;
        if desc.VendorId != AMD_VENDOR_ID || desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let adapter: IDXGIAdapter3 = adapter.cast().ok()?;
        let query = |group| {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            // SAFETY: node 0 exists on every adapter; the out pointer is a live local.
            unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }
                .ok()
                .map(|()| info.CurrentUsage)
        };
        return Some((
            query(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?,
            query(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?,
        ));
    }
    None
}

/// Samples every `every` and logs a «resources» line; the latest one is kept for the status request.
pub fn spawn(
    every: Duration,
    latest: Arc<Mutex<Sample>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("resources".into())
        .spawn(move || {
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    next += every;
                    let s = sample();
                    let mb = |b: u64| (b as f64 / 1_048_576.0 * 10.0).round() / 10.0;
                    info!(
                        working_set_mb = mb(s.working_set),
                        private_mb = mb(s.private_bytes),
                        handles = s.handles,
                        gdi = s.gdi_objects,
                        user = s.user_objects,
                        gpu_local_mb = s.gpu_local.map(mb),
                        gpu_non_local_mb = s.gpu_non_local.map(mb),
                        "resources"
                    );
                    if let Ok(mut l) = latest.lock() {
                        *l = s;
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_sees_this_process() {
        let s = sample();
        assert!(s.working_set > 0 && s.private_bytes > 0, "{s:?}");
        assert!(s.handles > 0, "{s:?}");
    }
}
