//! Frame capture with Desktop Duplication into a pool of textures that stay in video memory.
//!
//! The duplicated desktop texture belongs to the OS and may change once the frame is released, so every new frame
//! is copied on the GPU into the next texture of a small pool and the duplication frame is released right away.
//! Nothing is mapped or read back: the pool textures have no CPU access.

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{Error, Interface};

use crate::dxgi;

/// Textures in the pool. The encoder reads a texture a few frames at most after it was written.
pub const POOL_SIZE: usize = 4;

/// One captured frame. The texture stays valid until `POOL_SIZE` more frames have been captured.
#[derive(Debug, Clone, Copy)]
pub struct CapturedFrame<'a> {
    pub texture: &'a ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// When `AcquireNextFrame` returned.
    pub acquired: Instant,
    /// Time from the desktop present to the end of `AcquireNextFrame`, if the OS reported the present time.
    pub present_to_acquire_ms: Option<f64>,
    /// Presents the OS merged into this frame, more than 1 means frames were skipped.
    pub accumulated_frames: u32,
    /// CPU time of `CopyResource` and `ReleaseFrame`.
    pub copy_ms: f64,
    /// True when this is the previous frame again, see `DxgiCapture::last_frame`.
    pub repeated: bool,
}

pub enum CaptureEvent<'a> {
    Frame(CapturedFrame<'a>),
    /// No new desktop image within the timeout. Pointer-only updates count as a timeout too.
    Timeout,
    /// The duplication was recreated: mode change, secure desktop or full-screen switch. The next frame should be
    /// an IDR, and the size may have changed.
    Reinitialize,
}

#[derive(Debug)]
pub enum CaptureError {
    /// The GPU was reset or removed; the device and everything on it must be recreated.
    DeviceRemoved(Error),
    Failed(Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceRemoved(e) => write!(f, "device removed: {}", dxgi::describe(e)),
            Self::Failed(e) => write!(f, "capture failed: {}", dxgi::describe(e)),
        }
    }
}

impl std::error::Error for CaptureError {}

pub trait Capture {
    fn next_frame(&mut self, timeout: Duration) -> Result<CaptureEvent<'_>, CaptureError>;
    fn size(&self) -> (u32, u32);
}

/// Counters for reports.
#[derive(Debug, Clone, Default)]
pub struct CaptureCounters {
    pub frames: u64,
    pub timeouts: u64,
    pub pointer_only: u64,
    pub access_lost: u64,
    pub accumulated_extra: u64,
    pub pool_recreated: u64,
}

/// Facts that show the frame stays in video memory.
#[derive(Debug, Clone, Default)]
pub struct MemoryFacts {
    pub desktop_in_system_memory: bool,
    pub pool_usage: i32,
    pub pool_cpu_access_flags: u32,
    pub pool_bind_flags: u32,
    pub format: i32,
}

pub struct DxgiCapture {
    adapter: IDXGIAdapter1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output_index: u32,
    duplication: Option<IDXGIOutputDuplication>,
    pool: Vec<ID3D11Texture2D>,
    pool_key: (u32, u32, DXGI_FORMAT),
    mode: (u32, u32),
    next: usize,
    last: Option<usize>,
    last_acquired: Option<Instant>,
    qpc_frequency: f64,
    pub counters: CaptureCounters,
    pub memory: MemoryFacts,
}

fn qpc_now() -> i64 {
    let mut value = 0;
    // SAFETY: the out pointer refers to a live local; the call cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut value) };
    value
}

impl DxgiCapture {
    pub fn new(
        adapter: IDXGIAdapter1,
        device: ID3D11Device,
        output_index: u32,
    ) -> Result<Self, CaptureError> {
        // SAFETY: the device is valid.
        let context = unsafe { device.GetImmediateContext() }.map_err(CaptureError::Failed)?;
        let mut frequency = 0;
        // SAFETY: the out pointer refers to a live local.
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        let mut capture = Self {
            adapter,
            device,
            context,
            output_index,
            duplication: None,
            pool: Vec::with_capacity(POOL_SIZE),
            pool_key: (0, 0, DXGI_FORMAT(0)),
            mode: (0, 0),
            next: 0,
            last: None,
            last_acquired: None,
            qpc_frequency: frequency.max(1) as f64,
            counters: CaptureCounters::default(),
            memory: MemoryFacts::default(),
        };
        capture.reopen()?;
        Ok(capture)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    fn reopen(&mut self) -> Result<(), CaptureError> {
        self.duplication = None;
        let duplication =
            dxgi::duplicate(&self.adapter, &self.device, self.output_index).map_err(classify)?;
        // SAFETY: the duplication is valid.
        let desc = unsafe { duplication.GetDesc() };
        self.memory.desktop_in_system_memory = desc.DesktopImageInSystemMemory.as_bool();
        self.mode = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        self.duplication = Some(duplication);
        Ok(())
    }

    fn ensure_pool(&mut self, source: &D3D11_TEXTURE2D_DESC) -> Result<(), CaptureError> {
        let key = (source.Width, source.Height, source.Format);
        if self.pool.len() == POOL_SIZE && self.pool_key == key {
            return Ok(());
        }
        self.pool.clear();
        self.last = None;
        self.next = 0;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: source.Width,
            Height: source.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..POOL_SIZE {
            let mut texture = None;
            // SAFETY: the descriptor and the out pointer refer to live locals.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(classify)?;
            self.pool.push(texture.ok_or_else(|| {
                CaptureError::Failed(Error::from(windows::Win32::Foundation::E_POINTER))
            })?);
        }
        self.pool_key = key;
        self.counters.pool_recreated += 1;
        self.memory.pool_usage = desc.Usage.0;
        self.memory.pool_cpu_access_flags = desc.CPUAccessFlags;
        self.memory.pool_bind_flags = desc.BindFlags;
        self.memory.format = desc.Format.0;
        Ok(())
    }

    /// The most recent frame again, for the minimum frame rate on a static screen.
    pub fn last_frame(&self) -> Option<CapturedFrame<'_>> {
        let index = self.last?;
        Some(CapturedFrame {
            texture: &self.pool[index],
            width: self.pool_key.0,
            height: self.pool_key.1,
            acquired: Instant::now(),
            present_to_acquire_ms: None,
            accumulated_frames: 0,
            copy_ms: 0.0,
            repeated: true,
        })
    }

    pub fn last_acquired(&self) -> Option<Instant> {
        self.last_acquired
    }
}

fn classify(error: Error) -> CaptureError {
    if error.code() == DXGI_ERROR_DEVICE_REMOVED || error.code() == DXGI_ERROR_DEVICE_RESET {
        CaptureError::DeviceRemoved(error)
    } else {
        CaptureError::Failed(error)
    }
}

impl Capture for DxgiCapture {
    fn next_frame(&mut self, timeout: Duration) -> Result<CaptureEvent<'_>, CaptureError> {
        if self.duplication.is_none() {
            self.reopen()?;
            return Ok(CaptureEvent::Reinitialize);
        }
        let duplication = self.duplication.as_ref().expect("checked above");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let wait_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: both out pointers refer to live locals, no frame is held at this point.
        let acquired = unsafe { duplication.AcquireNextFrame(wait_ms, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                self.counters.timeouts += 1;
                return Ok(CaptureEvent::Timeout);
            }
            Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                self.counters.access_lost += 1;
                // The secure desktop may refuse a new duplication for a while; the next call retries.
                let _ = self.reopen();
                return Ok(CaptureEvent::Reinitialize);
            }
            Err(error) => return Err(classify(error)),
        }
        let acquired_at = Instant::now();
        let acquired_qpc = qpc_now();

        if info.LastPresentTime == 0 {
            // Only the pointer moved: no new desktop image.
            drop(resource);
            // SAFETY: a frame is held and must be released before the next acquire.
            unsafe { duplication.ReleaseFrame() }.map_err(classify)?;
            self.counters.pointer_only += 1;
            return Ok(CaptureEvent::Timeout);
        }

        let source = resource
            .as_ref()
            .and_then(|r| r.cast::<ID3D11Texture2D>().ok())
            .ok_or_else(|| {
                CaptureError::Failed(Error::from(windows::Win32::Foundation::E_NOINTERFACE))
            })?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the out pointer refers to a live local.
        unsafe { source.GetDesc(&mut desc) };
        if let Err(error) = self.ensure_pool(&desc) {
            let duplication = self.duplication.as_ref().expect("checked above");
            // SAFETY: a frame is held and must be released before the next acquire.
            let _ = unsafe { duplication.ReleaseFrame() };
            return Err(error);
        }

        let index = self.next;
        let copy_started = Instant::now();
        // SAFETY: both textures live on this device with the same size and format; the copy runs on the GPU.
        unsafe { self.context.CopyResource(&self.pool[index], &source) };
        drop(source);
        drop(resource);
        let duplication = self.duplication.as_ref().expect("checked above");
        // SAFETY: a frame is held; the GPU copy above is queued before the OS may update the desktop texture.
        unsafe { duplication.ReleaseFrame() }.map_err(classify)?;
        let copy_ms = copy_started.elapsed().as_secs_f64() * 1000.0;

        self.next = (index + 1) % POOL_SIZE;
        self.last = Some(index);
        self.last_acquired = Some(acquired_at);
        self.counters.frames += 1;
        self.counters.accumulated_extra += u64::from(info.AccumulatedFrames.saturating_sub(1));
        let present_to_acquire_ms =
            Some((acquired_qpc - info.LastPresentTime) as f64 * 1000.0 / self.qpc_frequency)
                .filter(|v| *v >= 0.0);

        Ok(CaptureEvent::Frame(CapturedFrame {
            texture: &self.pool[index],
            width: desc.Width,
            height: desc.Height,
            acquired: acquired_at,
            present_to_acquire_ms,
            accumulated_frames: info.AccumulatedFrames,
            copy_ms,
            repeated: false,
        }))
    }

    fn size(&self) -> (u32, u32) {
        self.mode
    }
}

/// Raises the system timer resolution for this process while alive, so short `AcquireNextFrame` timeouts wake up
/// on time instead of on the default 15.6 ms tick.
pub struct TimerResolution {
    period_ms: u32,
}

impl TimerResolution {
    pub fn new(period_ms: u32) -> Option<Self> {
        // SAFETY: no pointers; balanced by timeEndPeriod in Drop.
        let result = unsafe { windows::Win32::Media::timeBeginPeriod(period_ms) };
        (result == 0).then_some(Self { period_ms })
    }

    pub fn period_ms(&self) -> u32 {
        self.period_ms
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        // SAFETY: timeBeginPeriod succeeded with the same value.
        let _ = unsafe { windows::Win32::Media::timeEndPeriod(self.period_ms) };
    }
}
