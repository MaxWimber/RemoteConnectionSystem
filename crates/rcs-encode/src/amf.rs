//! AMD AMF through `native/amf_shim`. `amfrt64.dll` is loaded at run time, so this code also runs without AMD.

use std::ffi::{CStr, c_char, c_void};

use rcs_capture::CapturedFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

use crate::h264::{BT709_LIMITED, SpsRewriter};
use crate::{EncodeError, EncodedFrame, Encoder, SubmitStats};

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

/// Mirrors `RcsAmfEncoderConfig` in `amf_shim.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub input_width: i32,
    pub input_height: i32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate: i64,
    pub vbv_bits: i64,
    pub usage: i32,
    pub profile: i32,
    pub quality_preset: i32,
    pub idr_period: i32,
    pub intra_refresh_mbs: i32,
    pub query_timeout_ms: i32,
    pub enforce_hrd: i32,
    pub filler_data: i32,
    pub rate_control: i32,
    pub vbaq: i32,
    pub max_au_bits: i64,
    /// -1 keeps the default (disabled on the server); 0 disabled, 1 GOP aligned, 2 continuous.
    pub intra_refresh_mode: i32,
    /// `CODEC_AVC` or `CODEC_HEVC` (P9-3). The other fields take the AVC values; the shim maps them for HEVC.
    pub codec: i32,
}

pub const CODEC_AVC: i32 = 0;
pub const CODEC_HEVC: i32 = 1;

pub const USAGE_TRANSCODING: i32 = 0;
pub const USAGE_LOW_LATENCY: i32 = 2;
pub const USAGE_WEBCAM: i32 = 3;
pub const PROFILE_HIGH: i32 = 100;
pub const PROFILE_CONSTRAINED_BASELINE: i32 = 256;
pub const RATE_CONTROL_CBR: i32 = 1;
pub const RATE_CONTROL_PEAK_CONSTRAINED_VBR: i32 = 2;
pub const RATE_CONTROL_LATENCY_CONSTRAINED_VBR: i32 = 3;
pub const QUALITY_PRESET_BALANCED: i32 = 0;
pub const QUALITY_PRESET_SPEED: i32 = 1;
pub const QUALITY_PRESET_QUALITY: i32 = 2;
/// AMF memory types that appear in reports.
pub const MEMORY_HOST: i32 = 1;
pub const MEMORY_DX11: i32 = 3;

impl EncoderConfig {
    /// Low Latency usage (D-016), latency constrained VBR (D-018), High profile, defaults of the usage for the rest.
    pub fn low_latency(width: i32, height: i32, fps: i32, bitrate: i64) -> Self {
        Self {
            input_width: width,
            input_height: height,
            width,
            height,
            fps,
            bitrate,
            vbv_bits: 0,
            usage: USAGE_LOW_LATENCY,
            profile: PROFILE_HIGH,
            quality_preset: -1,
            idr_period: fps * 10,
            intra_refresh_mbs: -1,
            query_timeout_ms: 50,
            enforce_hrd: -1,
            filler_data: -1,
            // D-018: no frame larger than one frame interval of the bitrate, at the same mean quality as CBR.
            rate_control: RATE_CONTROL_LATENCY_CONSTRAINED_VBR,
            vbaq: -1,
            max_au_bits: 0,
            intra_refresh_mode: -1,
            codec: CODEC_AVC,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SubmitInfo {
    convert_ms: f64,
    input_memory: i32,
    output_memory: i32,
    input_full: i32,
}

#[repr(C)]
struct Packet {
    data: *const u8,
    size: usize,
    pts: i64,
    output_type: i32,
    memory: i32,
}

#[repr(C)]
struct RawEncoder {
    _private: [u8; 0],
}

const RCS_AMF_OK: i32 = 0;
const RCS_AMF_AGAIN: i32 = 1;
const OUTPUT_TYPE_IDR: i32 = 0;

unsafe extern "C" {
    fn rcs_amf_encoder_create(
        device: *mut c_void,
        config: *const EncoderConfig,
        message: *mut c_char,
        message_size: usize,
    ) -> *mut RawEncoder;
    fn rcs_amf_encoder_submit(
        encoder: *mut RawEncoder,
        texture: *mut c_void,
        pts: i64,
        force_idr: i32,
        info: *mut SubmitInfo,
    ) -> i32;
    fn rcs_amf_encoder_query(encoder: *mut RawEncoder, packet: *mut Packet) -> i32;
    fn rcs_amf_encoder_set_bitrate(encoder: *mut RawEncoder, bitrate: i64) -> i32;
    fn rcs_amf_encoder_error(encoder: *mut RawEncoder) -> *const c_char;
    fn rcs_amf_encoder_settings(encoder: *mut RawEncoder) -> *mut c_char;
    fn rcs_amf_encoder_destroy(encoder: *mut RawEncoder);
}

/// AMF encoder on the capture device. Not `Send`: it runs on the thread that created it.
pub struct AmfEncoder {
    raw: *mut RawEncoder,
    config: EncoderConfig,
    sps: SpsRewriter,
    sps_rewritten: u64,
}

impl AmfEncoder {
    pub fn new(device: &ID3D11Device, config: &EncoderConfig) -> Result<Self, EncodeError> {
        let mut message = [0 as c_char; 512];
        // SAFETY: the device and the config outlive the call; the message buffer size is passed with it.
        let raw = unsafe {
            rcs_amf_encoder_create(device.as_raw(), config, message.as_mut_ptr(), message.len())
        };
        if raw.is_null() {
            // SAFETY: the shim always writes a NUL-terminated message on failure, and the buffer starts zeroed.
            let text = unsafe { CStr::from_ptr(message.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(EncodeError(text));
        }
        Ok(Self {
            raw,
            config: *config,
            sps: SpsRewriter::default(),
            sps_rewritten: 0,
        })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn error(&self) -> EncodeError {
        // SAFETY: the encoder is alive; the string is owned by it and copied before the next call.
        let text = unsafe { CStr::from_ptr(rcs_amf_encoder_error(self.raw)) };
        EncodeError(text.to_string_lossy().into_owned())
    }
}

impl Drop for AmfEncoder {
    fn drop(&mut self) {
        // SAFETY: created by rcs_amf_encoder_create and destroyed once.
        unsafe { rcs_amf_encoder_destroy(self.raw) };
    }
}

impl Encoder for AmfEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame<'_>,
        pts: i64,
        force_idr: bool,
    ) -> Result<SubmitStats, EncodeError> {
        let mut info = SubmitInfo::default();
        // SAFETY: the texture lives on the device the encoder was created on and outlives the call; AMF wraps it
        // without taking it over.
        let status = unsafe {
            rcs_amf_encoder_submit(
                self.raw,
                frame.texture.as_raw(),
                pts,
                i32::from(force_idr),
                &mut info,
            )
        };
        if status != RCS_AMF_OK {
            return Err(self.error());
        }
        Ok(SubmitStats {
            convert_ms: info.convert_ms,
            input_in_video_memory: info.input_memory == MEMORY_DX11,
            converted_in_video_memory: info.output_memory == MEMORY_DX11,
            input_full: info.input_full as u32,
        })
    }

    fn poll(&mut self, out: &mut EncodedFrame) -> Result<bool, EncodeError> {
        let mut packet = Packet {
            data: std::ptr::null(),
            size: 0,
            pts: 0,
            output_type: -1,
            memory: 0,
        };
        // SAFETY: the encoder is alive and the packet points to a live local.
        let status = unsafe { rcs_amf_encoder_query(self.raw, &mut packet) };
        match status {
            RCS_AMF_OK => {}
            RCS_AMF_AGAIN => return Ok(false),
            _ => return Err(self.error()),
        }
        out.data.clear();
        if !packet.data.is_null() && packet.size > 0 {
            // SAFETY: the shim keeps the buffer alive until the next query, and size is its length in bytes.
            out.data
                .extend_from_slice(unsafe { std::slice::from_raw_parts(packet.data, packet.size) });
        }
        out.pts = packet.pts;
        out.keyframe = packet.output_type == OUTPUT_TYPE_IDR;
        if out.keyframe {
            // AMF 1.4.31 accepts the output colour properties but writes no colour description into the VUI,
            // so the SPS of every IDR gets BT.709 limited range here. Only IDR frames carry an SPS.
            self.sps_rewritten += self.sps.set_sps_colour(&mut out.data, BT709_LIMITED) as u64;
        }
        out.output_type = packet.output_type;
        Ok(true)
    }

    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
        // SAFETY: the encoder is alive.
        if unsafe { rcs_amf_encoder_set_bitrate(self.raw, i64::from(bits_per_second)) }
            != RCS_AMF_OK
        {
            return Err(self.error());
        }
        self.config.bitrate = i64::from(bits_per_second);
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "amf"
    }

    /// Settings read back from AMF after Init, as JSON.
    fn settings(&self) -> Option<String> {
        // SAFETY: the encoder is alive; the returned string is freed by the shim exactly once.
        unsafe {
            let text = rcs_amf_encoder_settings(self.raw);
            if text.is_null() {
                return None;
            }
            let json = CStr::from_ptr(text).to_string_lossy().into_owned();
            rcs_amf_free(text);
            Some(json)
        }
    }

    /// SPS units that got the BT.709 colour description, see `poll`.
    fn sps_rewritten(&self) -> u64 {
        self.sps_rewritten
    }
}
