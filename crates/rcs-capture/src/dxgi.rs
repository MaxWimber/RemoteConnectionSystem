//! DXGI adapters, D3D11 devices and Desktop Duplication.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11CreateDevice,
    ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutput6, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::{Error, Interface, PCWSTR, Result};

/// Makes desktop coordinates physical pixels. Without it a scaled display reports, for example, 1536x864
/// instead of 1920x1080. Call once at process start.
pub fn set_dpi_awareness() -> Result<()> {
    // SAFETY: no pointers are passed.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
}

/// Adapter facts for reports.
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub revision: u32,
    pub dedicated_video_memory: u64,
    pub shared_system_memory: u64,
    pub luid: u64,
    pub software: bool,
    pub driver_version: Option<String>,
    pub outputs: Vec<OutputInfo>,
}

/// One display connected to an adapter.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub index: u32,
    pub device_name: String,
    pub desktop: [i32; 4],
    pub attached: bool,
    pub rotation: i32,
    pub bits_per_color: Option<u32>,
    pub color_space: Option<i32>,
    /// Width, height and refresh rate in Hz from the display settings.
    pub mode: Option<(u32, u32, u32)>,
}

pub struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub info: AdapterInfo,
}

/// Result of a short Desktop Duplication run on one output.
#[derive(Debug, Clone, Default)]
pub struct DuplicationReport {
    pub output_index: u32,
    pub error: Option<String>,
    pub width: u32,
    pub height: u32,
    pub refresh: (u32, u32),
    pub format: i32,
    pub rotation: i32,
    pub desktop_in_system_memory: bool,
    pub frames: u32,
    pub timeouts: u32,
    pub access_lost: u32,
    pub acquire_ms: Vec<f64>,
    pub texture_format: Option<i32>,
    pub texture_bind_flags: Option<u32>,
    pub texture_misc_flags: Option<u32>,
    pub elapsed_ms: f64,
}

pub fn describe(error: &Error) -> String {
    format!("{:#010x} {}", error.code().0 as u32, error.message())
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

pub fn enumerate_adapters() -> Result<Vec<Adapter>> {
    // SAFETY: no arguments besides the requested interface.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapters = Vec::new();
    for index in 0.. {
        // SAFETY: the factory is valid; DXGI_ERROR_NOT_FOUND marks the end of the list.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error),
        };
        // SAFETY: the adapter is valid.
        let desc = unsafe { adapter.GetDesc1()? };
        // SAFETY: the IID pointer refers to a static GUID.
        let driver_version = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
            .ok()
            .map(|version| {
                let v = version as u64;
                format!(
                    "{}.{}.{}.{}",
                    v >> 48,
                    (v >> 32) & 0xffff,
                    (v >> 16) & 0xffff,
                    v & 0xffff
                )
            });
        let info = AdapterInfo {
            index,
            name: wide(&desc.Description),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            revision: desc.Revision,
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
            shared_system_memory: desc.SharedSystemMemory as u64,
            luid: ((desc.AdapterLuid.HighPart as u32 as u64) << 32)
                | desc.AdapterLuid.LowPart as u64,
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            driver_version,
            outputs: enumerate_outputs(&adapter),
        };
        adapters.push(Adapter { adapter, info });
    }
    Ok(adapters)
}

fn enumerate_outputs(adapter: &IDXGIAdapter1) -> Vec<OutputInfo> {
    let mut outputs = Vec::new();
    for index in 0.. {
        // SAFETY: the adapter is valid; any error ends the list.
        let Ok(output) = (unsafe { adapter.EnumOutputs(index) }) else {
            break;
        };
        // SAFETY: the output is valid.
        let Ok(desc) = (unsafe { output.GetDesc() }) else {
            continue;
        };
        // SAFETY: the cast succeeded, so the interface is valid.
        let desc1 = output
            .cast::<IDXGIOutput6>()
            .ok()
            .and_then(|o| unsafe { o.GetDesc1() }.ok());
        let r = desc.DesktopCoordinates;
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: the device name is NUL-terminated and dmSize is set as the API requires.
        let has_mode = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(desc.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
        }
        .as_bool();
        outputs.push(OutputInfo {
            index,
            device_name: wide(&desc.DeviceName),
            desktop: [r.left, r.top, r.right, r.bottom],
            attached: desc.AttachedToDesktop.as_bool(),
            rotation: desc.Rotation.0,
            bits_per_color: desc1.map(|d| d.BitsPerColor),
            color_space: desc1.map(|d| d.ColorSpace.0),
            mode: has_mode.then_some((
                mode.dmPelsWidth,
                mode.dmPelsHeight,
                mode.dmDisplayFrequency,
            )),
        });
    }
    outputs
}

/// Creates a D3D11 device on the adapter. Capture and the encoder share it from several threads,
/// so multithread protection is switched on.
pub fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, D3D_FEATURE_LEVEL)> {
    let mut device = None;
    let mut level = D3D_FEATURE_LEVEL::default();
    // SAFETY: all out pointers refer to live locals.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            None,
        )?;
    }
    let device = device.ok_or_else(|| Error::from(windows::Win32::Foundation::E_POINTER))?;
    // SAFETY: the device is valid; the previous state is irrelevant.
    unsafe {
        let _ = device
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true);
    }
    Ok((device, level))
}

fn duplicate(
    adapter: &IDXGIAdapter1,
    device: &ID3D11Device,
    output_index: u32,
) -> Result<IDXGIOutputDuplication> {
    // SAFETY: the adapter and the device are valid and belong together.
    unsafe {
        adapter
            .EnumOutputs(output_index)?
            .cast::<IDXGIOutput1>()?
            .DuplicateOutput(device)
    }
}

/// Acquires frames for `duration` and reports what the duplication delivered.
pub fn probe_duplication(
    adapter: &IDXGIAdapter1,
    device: &ID3D11Device,
    output_index: u32,
    duration: Duration,
) -> DuplicationReport {
    let mut report = DuplicationReport {
        output_index,
        ..Default::default()
    };
    let mut duplication = match duplicate(adapter, device, output_index) {
        Ok(duplication) => duplication,
        Err(error) => {
            report.error = Some(describe(&error));
            return report;
        }
    };
    // SAFETY: the duplication is valid.
    let desc = unsafe { duplication.GetDesc() };
    report.width = desc.ModeDesc.Width;
    report.height = desc.ModeDesc.Height;
    report.refresh = (
        desc.ModeDesc.RefreshRate.Numerator,
        desc.ModeDesc.RefreshRate.Denominator,
    );
    report.format = desc.ModeDesc.Format.0;
    report.rotation = desc.Rotation.0;
    report.desktop_in_system_memory = desc.DesktopImageInSystemMemory.as_bool();

    let started = Instant::now();
    while started.elapsed() < duration {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let before = Instant::now();
        // SAFETY: both out pointers refer to live locals.
        let acquired = unsafe { duplication.AcquireNextFrame(50, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {
                report
                    .acquire_ms
                    .push(before.elapsed().as_secs_f64() * 1000.0);
                report.frames += 1;
                if report.texture_format.is_none()
                    && let Some(texture) = resource
                        .as_ref()
                        .and_then(|r| r.cast::<ID3D11Texture2D>().ok())
                {
                    let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
                    // SAFETY: the out pointer refers to a live local.
                    unsafe { texture.GetDesc(&mut texture_desc) };
                    report.texture_format = Some(texture_desc.Format.0);
                    report.texture_bind_flags = Some(texture_desc.BindFlags);
                    report.texture_misc_flags = Some(texture_desc.MiscFlags);
                }
                drop(resource);
                // SAFETY: a frame is held, so it must be released before the next acquire.
                if let Err(error) = unsafe { duplication.ReleaseFrame() } {
                    report.error = Some(describe(&error));
                    break;
                }
            }
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => report.timeouts += 1,
            Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                report.access_lost += 1;
                match duplicate(adapter, device, output_index) {
                    Ok(next) => duplication = next,
                    Err(error) => {
                        report.error = Some(describe(&error));
                        break;
                    }
                }
            }
            Err(error) => {
                report.error = Some(describe(&error));
                break;
            }
        }
    }
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    report
}
