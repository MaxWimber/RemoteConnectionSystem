//! Screen capture.

pub mod capture;
pub mod dxgi;

pub use capture::{
    Capture, CaptureError, CaptureEvent, CapturedFrame, DxgiCapture, TimerResolution,
};
pub use dxgi::{AdapterInfo, DuplicationReport, OutputInfo};
