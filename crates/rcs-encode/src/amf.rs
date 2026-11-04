//! AMD AMF through `native/amf_shim`. `amfrt64.dll` is loaded at run time, so this code also runs without AMD.

use std::ffi::{CStr, c_char, c_void};

use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

/// Mirrors `RcsAmfProbeOptions` in `amf_shim.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProbeOptions {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub frames: i32,
    pub bitrate: i64,
}

type ProbeFn = unsafe extern "C" fn(*mut c_void, *const ProbeOptions) -> *mut c_char;

unsafe extern "C" {
    fn rcs_amf_probe(device: *mut c_void, options: *const ProbeOptions) -> *mut c_char;
    fn rcs_amf_ull_probe(device: *mut c_void, options: *const ProbeOptions) -> *mut c_char;
    fn rcs_amf_free(text: *mut c_char);
}

fn call(probe: ProbeFn, device: &ID3D11Device, options: &ProbeOptions) -> Option<String> {
    // SAFETY: the device and the options outlive the call. The shim returns a NUL-terminated string allocated
    // by itself or null, and the string is freed by the shim exactly once.
    unsafe {
        let text = probe(device.as_raw(), options);
        if text.is_null() {
            return None;
        }
        let report = CStr::from_ptr(text).to_string_lossy().into_owned();
        rcs_amf_free(text);
        Some(report)
    }
}

/// Runs the AMF probe on `device` and returns its JSON report, or `None` if the shim ran out of memory.
pub fn probe(device: &ID3D11Device, options: &ProbeOptions) -> Option<String> {
    call(rcs_amf_probe, device, options)
}

/// Runs the Ultra Low Latency probe on `device` and returns its JSON report.
pub fn ull_probe(device: &ID3D11Device, options: &ProbeOptions) -> Option<String> {
    call(rcs_amf_ull_probe, device, options)
}
