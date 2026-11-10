//! Testing only (P9-1): lists active output devices, or makes one the default for every role, so the agent's switch
//! to a new default device can be checked without a person at the server. `IPolicyConfig` is undocumented but has
//! been stable since Windows 7; Windows' own sound settings use it.
//!
//! Usage: switch_default            list devices, the default first
//!        switch_default ID         make ID the default output

use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, ERole, IMMDeviceEnumerator, MMDeviceEnumerator, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn get_mix_format(&self) -> HRESULT;
    fn get_device_format(&self) -> HRESULT;
    fn reset_device_format(&self) -> HRESULT;
    fn set_device_format(&self) -> HRESULT;
    fn get_processing_period(&self) -> HRESULT;
    fn set_processing_period(&self) -> HRESULT;
    fn get_share_mode(&self) -> HRESULT;
    fn set_share_mode(&self) -> HRESULT;
    fn get_property_value(&self) -> HRESULT;
    fn set_property_value(&self) -> HRESULT;
    fn set_default_endpoint(&self, id: PCWSTR, role: ERole) -> HRESULT;
}

const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn main() -> windows::core::Result<()> {
    // SAFETY: COM for this thread; every pointer below is valid for the call that takes it.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        if let Some(id) = std::env::args().nth(1) {
            let policy: IPolicyConfig = CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
            let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
            for role in [eConsole, eMultimedia, eCommunications] {
                policy
                    .set_default_endpoint(PCWSTR(wide.as_ptr()), role)
                    .ok()?;
            }
            println!("default now {}", rcs_audio_id()?);
            return Ok(());
        }
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        println!("default {}", rcs_audio_id()?);
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devices.GetCount()? {
            let id = devices.Item(i)?.GetId()?;
            println!("{}", id.to_string().unwrap_or_default());
            CoTaskMemFree(Some(id.0 as *const _));
        }
    }
    Ok(())
}

fn rcs_audio_id() -> windows::core::Result<String> {
    rcs_audio::default_device_id()
        .map_err(|e| windows::core::Error::new(HRESULT(-1), e.to_string()))
}
