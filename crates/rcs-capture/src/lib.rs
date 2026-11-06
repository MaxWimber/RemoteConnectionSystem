//! Screen capture.

pub mod capture;
pub mod desktop;
pub mod dxgi;

pub use capture::{
    Capture, CaptureError, CaptureEvent, CapturedFrame, DxgiCapture, Pointer, PointerShape,
    TimerResolution,
};
pub use dxgi::{AdapterInfo, DuplicationReport, OutputInfo};
