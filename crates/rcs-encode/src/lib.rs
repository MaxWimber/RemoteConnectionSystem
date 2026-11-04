//! Video encoders.

pub mod amf;
pub mod h264;
pub mod mf;

use rcs_capture::CapturedFrame;

#[derive(Debug, Clone)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EncodeError {}

/// One access unit in Annex B. The buffer is reused between frames, so polling allocates only when a frame is
/// larger than any before it.
#[derive(Debug, Clone, Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts: i64,
    pub keyframe: bool,
    /// Backend specific picture type, for AMF `AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_ENUM`.
    pub output_type: i32,
}

/// What a submit measured.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubmitStats {
    pub convert_ms: f64,
    pub input_in_video_memory: bool,
    pub converted_in_video_memory: bool,
    pub input_full: u32,
}

pub trait Encoder {
    /// Submits one frame. `pts` must grow with every call, also for repeated frames.
    fn encode(
        &mut self,
        frame: &CapturedFrame<'_>,
        pts: i64,
        force_idr: bool,
    ) -> Result<SubmitStats, EncodeError>;
    /// Writes the next encoded frame into `out` and returns true, or returns false when none is ready yet.
    fn poll(&mut self, out: &mut EncodedFrame) -> Result<bool, EncodeError>;
    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError>;
}
