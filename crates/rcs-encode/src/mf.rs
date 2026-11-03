//! Media Foundation: the `mf` backend for machines without AMF, see D-010.

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MF_API_VERSION, MF_SDK_VERSION, MF_TRANSFORM_FLAGS_Attribute, MFMediaType_Video,
    MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, PWSTR, Result};

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

/// Lists hardware encoders first, then software ones.
pub fn h264_encoders(_runtime: &Runtime) -> Result<Vec<EncoderInfo>> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
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
            enumerate(flags, &output)?
                .iter()
                .map(|activate| describe(activate, hardware)),
        );
    }
    Ok(encoders)
}

fn enumerate(flags: MFT_ENUM_FLAG, output: &MFT_REGISTER_TYPE_INFO) -> Result<Vec<IMFActivate>> {
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out pointers refer to live locals; the array is released below.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
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
