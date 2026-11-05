//! Media Foundation: the `mf` backend for machines without AMF, see D-010.
//!
//! The frame stays in video memory: the D3D11 video processor turns the BGRA texture into NV12 (BT.709, limited
//! range) on the GPU, and the hardware H.264 transform reads that texture through a DXGI device manager on the same
//! device. Hardware transforms are asynchronous: an input goes in after `METransformNeedInput`, an output comes out
//! after `METransformHaveOutput`. Software transforms would need the frame in system memory, so they are not used.

use std::mem::ManuallyDrop;
use std::time::Instant;

use rcs_capture::CapturedFrame;
use windows::Win32::Foundation::{RECT, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_API_VERSION, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_SA_D3D11_AWARE, MF_SDK_VERSION, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_TRANSFORM_FLAGS_Attribute, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_16_235, MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFShutdown,
    MFShutdownObject, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface, PWSTR, Result};

use crate::amf::{EncoderConfig, PROFILE_CONSTRAINED_BASELINE};
use crate::h264::{BT709_LIMITED, SpsRewriter};
use crate::{EncodeError, EncodedFrame, Encoder, SubmitStats};

/// One H.264 encoder transform registered in the system.
#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub name: Option<String>,
    pub hardware: bool,
    pub vendor_id: Option<String>,
    pub adapter_luid: Option<u64>,
    pub transform_flags: Option<u32>,
}

/// Keeps Media Foundation started for the lifetime of the value.
pub struct Runtime(());

impl Runtime {
    pub fn start() -> Result<Self> {
        // SAFETY: balanced by MFShutdown in Drop.
        unsafe { MFStartup((MF_SDK_VERSION << 16) | MF_API_VERSION, MFSTARTUP_NOSOCKET)? };
        Ok(Self(()))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: MFStartup succeeded in `start`.
        let _ = unsafe { MFShutdown() };
    }
}

const H264_OUTPUT: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
    guidMajorType: MFMediaType_Video,
    guidSubtype: MFVideoFormat_H264,
};

/// Lists hardware encoders first, then software ones.
pub fn h264_encoders(_runtime: &Runtime) -> Result<Vec<EncoderInfo>> {
    let mut encoders = Vec::new();
    let passes = [
        (MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, true),
        (
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            false,
        ),
    ];
    for (flags, hardware) in passes {
        encoders.extend(
            enumerate(flags, None, &H264_OUTPUT)?
                .iter()
                .map(|activate| describe(activate, hardware)),
        );
    }
    Ok(encoders)
}

fn enumerate(
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: &MFT_REGISTER_TYPE_INFO,
) -> Result<Vec<IMFActivate>> {
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out pointers refer to live locals; the array is released below.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            input.map(|i| i as *const _),
            Some(output),
            &mut array,
            &mut count,
        )?
    };
    if array.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: MFTEnumEx returned `count` initialized entries. Each one is moved out exactly once, then the
    // array itself is freed with CoTaskMemFree as the documentation requires.
    let activates = unsafe {
        let list = (0..count as usize)
            .filter_map(|i| array.add(i).read())
            .collect();
        CoTaskMemFree(Some(array as *const _));
        list
    };
    Ok(activates)
}

fn describe(activate: &IMFActivate, hardware: bool) -> EncoderInfo {
    EncoderInfo {
        name: string(activate, &MFT_FRIENDLY_NAME_Attribute),
        hardware,
        vendor_id: string(activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
        // SAFETY: the activate object is valid.
        adapter_luid: unsafe { activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID) }.ok(),
        // SAFETY: the activate object is valid.
        transform_flags: unsafe { activate.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute) }.ok(),
    }
}

fn string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut text = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: the out pointers refer to live locals; the string is freed with CoTaskMemFree.
    unsafe {
        activate
            .GetAllocatedString(key, &mut text, &mut length)
            .ok()?;
        let value = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        value
    }
}

/// NV12 surfaces handed to the transform. The transform may still hold a sample while later frames are submitted.
const SURFACES: usize = 4;
/// Input views kept for capture pool textures, see `DxgiCapture`.
const INPUT_VIEWS: usize = 8;
/// How long `encode` waits for the transform to ask for input.
const INPUT_WAIT_MS: u128 = 200;

/// The output view and the sample both hold a reference to the NV12 texture.
struct Surface {
    view: ID3D11VideoProcessorOutputView,
    sample: IMFSample,
}

fn err(context: &str, error: windows::core::Error) -> EncodeError {
    EncodeError(format!("{context}: {error}"))
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

fn variant_true() -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_TRUE,
                },
            }),
        },
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Hardware H.264 transform of Media Foundation on the capture device. Not `Send`, like `AmfEncoder`.
pub struct MfEncoder {
    video_context: ID3D11VideoContext1,
    video_device: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_views: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    surfaces: Vec<Surface>,
    next_surface: usize,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    need_input: u32,
    have_output: u32,
    provides_samples: bool,
    output_size: u32,
    output_sample: Option<IMFSample>,
    frame_duration: i64,
    name: String,
    notes: Vec<String>,
    config: EncoderConfig,
    sps: SpsRewriter,
    sps_rewritten: u64,
    // Last field: Media Foundation shuts down after everything above is released.
    _runtime: Runtime,
}

impl MfEncoder {
    pub fn new(
        device: &ID3D11Device,
        config: &EncoderConfig,
    ) -> std::result::Result<Self, EncodeError> {
        let runtime = Runtime::start().map_err(|e| err("MFStartup", e))?;
        let (input_w, input_h) = (config.input_width as u32, config.input_height as u32);
        let (width, height) = (config.width as u32, config.height as u32);
        let fps = config.fps.max(1) as u32;

        // SAFETY: plain COM calls on a valid device; every returned object is owned by the encoder.
        let (video_device, video_context, enumerator, processor) = unsafe {
            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| err("the device has no video support", e))?;
            let context = device
                .GetImmediateContext()
                .map_err(|e| err("context", e))?;
            let video_context: ID3D11VideoContext1 =
                context.cast().map_err(|e| err("ID3D11VideoContext1", e))?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: fps,
                    Denominator: 1,
                },
                InputWidth: input_w,
                InputHeight: input_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: fps,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| err("video processor enumerator", e))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| err("video processor", e))?;
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            let full = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
            (video_device, video_context, enumerator, processor)
        };

        let mut notes = Vec::new();
        let (transform, name) = activate_transform(device)?;
        // SAFETY: COM calls on the transform created above.
        let (events, manager) = unsafe {
            let attributes = transform
                .GetAttributes()
                .map_err(|e| err("attributes", e))?;
            let asynchronous = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if asynchronous {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| err("async unlock", e))?;
            }
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(EncodeError(format!("{name} does not take D3D11 textures")));
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| err("DXGI device manager", e))?;
            let manager = manager.ok_or_else(|| EncodeError("no DXGI device manager".into()))?;
            manager
                .ResetDevice(device, token)
                .map_err(|e| err("ResetDevice", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(|e| err("MFT_MESSAGE_SET_D3D_MANAGER", e))?;
            let events = if asynchronous {
                Some(
                    transform
                        .cast::<IMFMediaEventGenerator>()
                        .map_err(|e| err("events", e))?,
                )
            } else {
                None
            };
            (events, manager)
        };

        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(codec) = &codec {
            let gop = config.idr_period.max(1) as u32;
            let values: [(&str, &GUID, VARIANT); 5] = [
                (
                    "rate_control_cbr",
                    &CODECAPI_AVEncCommonRateControlMode,
                    variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                ),
                (
                    "mean_bitrate",
                    &CODECAPI_AVEncCommonMeanBitRate,
                    variant_u32(config.bitrate.clamp(0, u32::MAX as i64) as u32),
                ),
                ("low_latency", &CODECAPI_AVLowLatencyMode, variant_true()),
                ("gop_size", &CODECAPI_AVEncMPVGOPSize, variant_u32(gop)),
                (
                    "b_frames",
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    variant_u32(0),
                ),
            ];
            for (label, key, value) in &values {
                // SAFETY: the key and the value are live for the call; the values hold no pointers.
                let outcome = unsafe { codec.SetValue(*key, value) };
                notes.push(match outcome {
                    Ok(()) => format!("{label}: ok"),
                    Err(e) => format!("{label}: {e}"),
                });
            }
        } else {
            notes.push("ICodecAPI: not available".into());
        }

        // SAFETY: media types are fresh objects; the transform copies what it needs.
        unsafe {
            let output = media_type(&MFVideoFormat_H264, width, height, fps)?;
            let profile = if config.profile == PROFILE_CONSTRAINED_BASELINE {
                eAVEncH264VProfile_ConstrainedBase.0
            } else {
                eAVEncH264VProfile_High.0
            };
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)
                .map_err(|e| err("profile", e))?;
            output
                .SetUINT32(
                    &MF_MT_AVG_BITRATE,
                    config.bitrate.clamp(0, u32::MAX as i64) as u32,
                )
                .map_err(|e| err("bitrate", e))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| err("SetOutputType", e))?;
            let input = media_type(&MFVideoFormat_NV12, width, height, fps)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| err("SetInputType", e))?;
        }

        // SAFETY: COM calls on the transform; the stream info is a plain struct.
        let (provides_samples, output_size) = unsafe {
            let info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| err("GetOutputStreamInfo", e))?;
            (
                info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                info.cbSize.max(4 << 20),
            )
        };

        let surfaces = (0..SURFACES)
            .map(|_| create_surface(device, &video_device, &enumerator, width, height))
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // SAFETY: streaming starts once the types are set.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("BEGIN_STREAMING", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("START_OF_STREAM", e))?;
        }

        Ok(Self {
            video_context,
            video_device,
            enumerator,
            processor,
            input_views: Vec::with_capacity(INPUT_VIEWS),
            surfaces,
            next_surface: 0,
            transform,
            events,
            codec,
            _manager: manager,
            need_input: 0,
            have_output: 0,
            provides_samples,
            output_size,
            output_sample: None,
            frame_duration: 10_000_000 / i64::from(fps),
            name,
            notes,
            config: *config,
            sps: SpsRewriter::default(),
            sps_rewritten: 0,
            _runtime: runtime,
        })
    }

    /// Takes queued events: input requests and ready outputs.
    fn pump(&mut self) -> std::result::Result<(), EncodeError> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        loop {
            // SAFETY: the generator is alive; NO_WAIT never blocks.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // SAFETY: the event is alive.
                    let kind = unsafe { event.GetType() }.map_err(|e| err("event type", e))?;
                    if kind == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.have_output += 1;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(err("GetEvent", e)),
            }
        }
    }

    fn input_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> std::result::Result<ID3D11VideoProcessorInputView, EncodeError> {
        if let Some((_, view)) = self.input_views.iter().find(|(t, _)| t == texture) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        // SAFETY: the texture lives on the device of the video processor; the out pointer is a live local.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|e| err("input view", e))?;
        let view = view.ok_or_else(|| EncodeError("no input view".into()))?;
        if self.input_views.len() == INPUT_VIEWS {
            self.input_views.remove(0);
        }
        self.input_views.push((texture.clone(), view.clone()));
        Ok(view)
    }
}

fn activate_transform(
    device: &ID3D11Device,
) -> std::result::Result<(IMFTransform, String), EncodeError> {
    // SAFETY: COM calls on the device; the descriptor is a plain struct.
    let luid = unsafe {
        device
            .cast::<IDXGIDevice>()
            .and_then(|d| d.GetAdapter())
            .and_then(|a| a.GetDesc())
            .map(|d| pack(d.AdapterLuid.HighPart as u32, d.AdapterLuid.LowPart))
            .ok()
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let activates = enumerate(
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        &H264_OUTPUT,
    )
    .map_err(|e| err("MFTEnumEx", e))?;
    // The transform of the capture adapter first, any other hardware one after it.
    let chosen = activates
        .iter()
        .find(|a| luid.is_some() && unsafe { a.GetUINT64(&MFT_ENUM_ADAPTER_LUID) }.ok() == luid)
        .or_else(|| activates.first())
        .ok_or_else(|| EncodeError("no hardware H.264 encoder in Media Foundation".into()))?;
    let name = string(chosen, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "unknown".into());
    // SAFETY: the activate object is valid; the transform is owned by the caller.
    let transform = unsafe { chosen.ActivateObject::<IMFTransform>() }
        .map_err(|e| err(&format!("activate {name}"), e))?;
    Ok((transform, name))
}

/// # Safety
/// Plain Media Foundation calls; the caller owns the returned type.
unsafe fn media_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> std::result::Result<IMFMediaType, EncodeError> {
    // SAFETY: the media type is a fresh object owned here.
    unsafe {
        let t = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
        let set =
            |key: &GUID, value: u32| t.SetUINT32(key, value).map_err(|e| err("media type", e));
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| err("major type", e))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(|e| err("subtype", e))?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))
            .map_err(|e| err("frame size", e))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))
            .map_err(|e| err("frame rate", e))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
            .map_err(|e| err("aspect", e))?;
        set(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        set(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        set(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        set(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(t)
    }
}

fn create_surface(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
) -> std::result::Result<Surface, EncodeError> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: the descriptor and the out pointer are live locals; no initial data.
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.is_err() {
        // Not every driver allows the encoder bind flag on a render target; the transform copies then.
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        // SAFETY: as above.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| err("NV12 texture", e))?;
    }
    let texture = texture.ok_or_else(|| EncodeError("no NV12 texture".into()))?;
    let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut view = None;
    // SAFETY: the texture belongs to the device of the video processor; the out pointer is a live local.
    unsafe {
        video_device.CreateVideoProcessorOutputView(
            &texture,
            enumerator,
            &view_desc,
            Some(&mut view),
        )
    }
    .map_err(|e| err("output view", e))?;
    let view = view.ok_or_else(|| EncodeError("no output view".into()))?;
    // SAFETY: the buffer wraps the texture without copying; the sample keeps both alive.
    let sample = unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)
            .map_err(|e| err("MFCreateDXGISurfaceBuffer", e))?;
        let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
        sample.AddBuffer(&buffer).map_err(|e| err("AddBuffer", e))?;
        sample
    };
    Ok(Surface { view, sample })
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: COM calls on live objects; errors during shutdown are irrelevant.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if self.events.is_some() {
                let _ = MFShutdownObject(&self.transform);
            }
        }
    }
}

impl Encoder for MfEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame<'_>,
        pts: i64,
        force_idr: bool,
    ) -> std::result::Result<SubmitStats, EncodeError> {
        let view = self.input_view(frame.texture)?;
        let surface = &self.surfaces[self.next_surface];
        let convert_started = Instant::now();
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ..Default::default()
        }];
        // SAFETY: the processor, both views and their textures are alive and belong to one device.
        let blt = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &surface.view, 0, &streams)
        };
        // SAFETY: releases the reference placed in the stream above, exactly once.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blt.map_err(|e| err("VideoProcessorBlt", e))?;
        let convert_ms = convert_started.elapsed().as_secs_f64() * 1000.0;

        if force_idr && let Some(codec) = &self.codec {
            // SAFETY: the key and the value are live for the call.
            unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
                .map_err(|e| err("force keyframe", e))?;
        }

        if self.events.is_some() {
            let waited = Instant::now();
            while self.need_input == 0 {
                self.pump()?;
                if self.need_input > 0 {
                    break;
                }
                if waited.elapsed().as_millis() > INPUT_WAIT_MS {
                    return Err(EncodeError("the transform asks for no input".into()));
                }
                std::thread::yield_now();
            }
            self.need_input -= 1;
        }
        let sample = self.surfaces[self.next_surface].sample.clone();
        // SAFETY: the sample wraps one of our NV12 textures, which outlives the transform's use of it.
        unsafe {
            sample
                .SetSampleTime(pts)
                .map_err(|e| err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.frame_duration)
                .map_err(|e| err("SetSampleDuration", e))?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| err("ProcessInput", e))?;
        }
        self.next_surface = (self.next_surface + 1) % self.surfaces.len();
        Ok(SubmitStats {
            convert_ms,
            input_in_video_memory: true,
            converted_in_video_memory: true,
            input_full: 0,
        })
    }

    fn poll(&mut self, out: &mut EncodedFrame) -> std::result::Result<bool, EncodeError> {
        if self.events.is_some() {
            self.pump()?;
            if self.have_output == 0 {
                return Ok(false);
            }
        }
        if !self.provides_samples && self.output_sample.is_none() {
            // SAFETY: fresh objects owned by the encoder, reused for every output.
            self.output_sample = Some(unsafe {
                let buffer =
                    MFCreateMemoryBuffer(self.output_size).map_err(|e| err("output buffer", e))?;
                let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
                sample.AddBuffer(&buffer).map_err(|e| err("AddBuffer", e))?;
                sample
            });
        }
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(self.output_sample.clone()),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: the buffer array is a live local; ownership of returned objects is taken below.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        // SAFETY: each field is taken out exactly once.
        let (sample, events) = unsafe {
            (
                ManuallyDrop::take(&mut buffers[0].pSample),
                ManuallyDrop::take(&mut buffers[0].pEvents),
            )
        };
        drop(events);
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The transform settled its output type; take the one it offers and ask again later.
                // SAFETY: COM calls on the transform.
                unsafe {
                    let offered = self
                        .transform
                        .GetOutputAvailableType(0, 0)
                        .map_err(|e| err("GetOutputAvailableType", e))?;
                    self.transform
                        .SetOutputType(0, &offered, 0)
                        .map_err(|e| err("SetOutputType after a stream change", e))?;
                }
                self.have_output = self.have_output.saturating_sub(1);
                return Ok(false);
            }
            Err(e) => return Err(err("ProcessOutput", e)),
        }
        self.have_output = self.have_output.saturating_sub(1);
        let sample = sample.ok_or_else(|| EncodeError("ProcessOutput gave no sample".into()))?;
        out.data.clear();
        // SAFETY: the sample and its buffer are alive; the buffer is unlocked before it is released.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| err("ConvertToContiguousBuffer", e))?;
            let mut data = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut length))
                .map_err(|e| err("Lock", e))?;
            if !data.is_null() {
                out.data
                    .extend_from_slice(std::slice::from_raw_parts(data, length as usize));
            }
            let _ = buffer.Unlock();
            if !self.provides_samples {
                let _ = buffer.SetCurrentLength(0);
            }
            out.pts = sample.GetSampleTime().unwrap_or(0);
            out.keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        }
        out.output_type = if out.keyframe { 0 } else { 2 };
        if out.keyframe {
            self.sps_rewritten += self.sps.set_sps_colour(&mut out.data, BT709_LIMITED) as u64;
        }
        Ok(true)
    }

    fn set_bitrate(&mut self, bits_per_second: u32) -> std::result::Result<(), EncodeError> {
        let Some(codec) = &self.codec else {
            return Err(EncodeError("the transform has no ICodecAPI".into()));
        };
        // SAFETY: the key and the value are live for the call.
        unsafe {
            codec.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(bits_per_second),
            )
        }
        .map_err(|e| err("mean bitrate", e))?;
        self.config.bitrate = i64::from(bits_per_second);
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "mf"
    }

    fn settings(&self) -> Option<String> {
        let notes: Vec<String> = self.notes.iter().map(|n| json_string(n)).collect();
        Some(format!(
            "{{\"backend\":\"mf\",\"transform\":{},\"asynchronous\":{},\"provides_samples\":{},\
             \"width\":{},\"height\":{},\"bitrate\":{},\"rate_control\":\"cbr\",\"codec_api\":[{}]}}",
            json_string(&self.name),
            self.events.is_some(),
            self.provides_samples,
            self.config.width,
            self.config.height,
            self.config.bitrate,
            notes.join(",")
        ))
    }

    fn sps_rewritten(&self) -> u64 {
        self.sps_rewritten
    }
}
