//! Video encoders.

pub mod amf;
pub mod h264;
pub mod mf;

use rcs_capture::CapturedFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

#[derive(Debug, Clone)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EncodeError {}

impl EncodeError {
    /// AMF has no encoder for this process. On a card with one encoder (RX 560) that means another process holds
    /// it (D-028).
    pub fn amf_busy(&self) -> bool {
        self.0.contains("AMF_ENCODER_NOT_PRESENT")
    }
}

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
    /// `amf` or `mf`, for logs and reports.
    fn backend(&self) -> &'static str;
    /// Settings the backend actually uses, as JSON.
    fn settings(&self) -> Option<String> {
        None
    }
    /// SPS units that got the BT.709 colour description (D-017).
    fn sps_rewritten(&self) -> u64 {
        0
    }
}

/// The `media.encoder` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// AMF, then Media Foundation.
    Auto,
    Amf,
    Mf,
}

impl Backend {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "amf" => Some(Self::Amf),
            "mf" => Some(Self::Mf),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Amf => "amf",
            Self::Mf => "mf",
        }
    }
}

/// Opens the encoder for `backend` on the capture device. An explicit backend that fails is an error, it never
/// falls back silently; `Auto` tries AMF first and says in the error why both failed. A busy AMF is not a reason
/// for Media Foundation: its hardware MFT on an AMD card is the same encoder (D-028).
pub fn open(
    backend: Backend,
    device: &ID3D11Device,
    config: &amf::EncoderConfig,
) -> Result<Box<dyn Encoder>, EncodeError> {
    match backend {
        Backend::Amf => Ok(Box::new(amf::AmfEncoder::new(device, config)?)),
        Backend::Mf => Ok(Box::new(mf::MfEncoder::new(device, config)?)),
        Backend::Auto => match amf::AmfEncoder::new(device, config) {
            Ok(encoder) => Ok(Box::new(encoder)),
            Err(amf_error) if amf_error.amf_busy() => {
                Err(EncodeError(format!("AMF busy: {amf_error}")))
            }
            Err(amf_error) => match mf::MfEncoder::new(device, config) {
                Ok(encoder) => Ok(Box::new(encoder)),
                Err(mf_error) => Err(EncodeError(format!(
                    "AMF: {amf_error}; Media Foundation: {mf_error}"
                ))),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_amf_is_told_by_its_result_name() {
        assert!(EncodeError("encoder Init: 36 AMF_ENCODER_NOT_PRESENT".into()).amf_busy());
        assert!(
            EncodeError("AMF busy: encoder Init: 36 AMF_ENCODER_NOT_PRESENT".into()).amf_busy()
        );
        assert!(!EncodeError("encoder Init: 4 AMF_NOT_SUPPORTED".into()).amf_busy());
    }
}
