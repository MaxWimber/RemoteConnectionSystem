//! WASAPI loopback capture in shared mode. Windows converts the mix to 48 kHz float stereo itself
//! (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`), so the device's own format does not matter.

use std::fmt;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::{CHANNELS, SAMPLE_RATE};

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: no default output device.
const E_NOTFOUND: windows::core::HRESULT = windows::core::HRESULT(0x8007_0490_u32 as i32);
/// `WAVE_FORMAT_IEEE_FLOAT`.
const FORMAT_FLOAT: u16 = 3;
/// The shared buffer: 100 ms in 100 ns units. The thread reads it every few milliseconds.
const BUFFER_100NS: i64 = 1_000_000;
/// A packet older than this has a broken position; its age is taken as zero.
const MAX_AGE: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum AudioError {
    /// No output device at all.
    NoDevice,
    /// The device went away or changed its format: open again.
    Invalidated,
    /// The Opus encoder refused its settings.
    Codec(String),
    Other(windows::core::Error),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no output device"),
            Self::Invalidated => f.write_str("the output device went away"),
            Self::Codec(e) => write!(f, "opus: {e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<opus::Error> for AudioError {
    fn from(e: opus::Error) -> Self {
        Self::Codec(e.to_string())
    }
}

impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        match e.code() {
            AUDCLNT_E_DEVICE_INVALIDATED => Self::Invalidated,
            E_NOTFOUND => Self::NoDevice,
            _ => Self::Other(e),
        }
    }
}

/// COM for the calling thread, multithreaded apartment; left on drop.
pub struct ComApartment(());

impl ComApartment {
    pub fn enter() -> windows::core::Result<Self> {
        // SAFETY: plain COM initialisation of this thread; balanced by `CoUninitialize` in `Drop`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(()))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: this thread entered COM in `enter`.
        unsafe { CoUninitialize() };
    }
}

fn default_device() -> Result<IMMDevice, AudioError> {
    // SAFETY: COM is initialised on this thread (`ComApartment`); the enumerator is a regular COM object.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    // SAFETY: a plain query of the enumerator created above.
    Ok(unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?)
}

fn device_id(device: &IMMDevice) -> Result<String, AudioError> {
    // SAFETY: `GetId` returns a string allocated with CoTaskMemAlloc that we own: read it, then free it.
    unsafe {
        let id = device.GetId()?;
        let text = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(text)
    }
}

/// The id of the default output device now, to notice when the user switches it.
pub fn default_device_id() -> Result<String, AudioError> {
    device_id(&default_device()?)
}

/// Loopback capture of the default output device as it was when opened.
pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    id: String,
    qpc_frequency: u64,
}

impl Loopback {
    pub fn open() -> Result<Self, AudioError> {
        let device = default_device()?;
        let id = device_id(&device)?;
        // SAFETY: activation of a COM interface on a device returned by the enumerator.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let block = (CHANNELS * size_of::<f32>()) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: FORMAT_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // SAFETY: `format` lives through the call; shared mode needs no periodicity.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_100NS,
                0,
                &format,
                None,
            )
        }?;
        // SAFETY: the client is initialised, so the capture service exists.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }?;
        let mut frequency = 0i64;
        // SAFETY: writes one integer.
        unsafe { QueryPerformanceFrequency(&mut frequency) }?;
        // SAFETY: the client is initialised.
        unsafe { client.Start() }?;
        Ok(Self {
            client,
            capture,
            id,
            qpc_frequency: frequency.max(1) as u64,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.id
    }

    /// Hands every packet waiting in the buffer to `each`: interleaved stereo samples (`None` for silence), the
    /// number of frames, and when its first frame went through the mixer.
    pub fn read(
        &mut self,
        mut each: impl FnMut(Option<&[f32]>, usize, Instant),
    ) -> Result<(), AudioError> {
        loop {
            // SAFETY: the capture client belongs to a started stream.
            if unsafe { self.capture.GetNextPacketSize() }? == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut position = 0u64;
            // SAFETY: every out pointer is valid; the buffer stays ours until `ReleaseBuffer`.
            unsafe {
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut position),
                )
            }?;
            let flags = flags as i32;
            let at = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 != 0 {
                Instant::now()
            } else {
                self.instant_of(position)
            };
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 != 0 || data.is_null();
            let samples = (!silent).then(|| {
                // SAFETY: WASAPI hands `frames` frames of the format we asked for, 2 x f32 each, aligned; the
                // slice is dropped before `ReleaseBuffer`.
                unsafe {
                    std::slice::from_raw_parts(data as *const f32, frames as usize * CHANNELS)
                }
            });
            each(samples, frames as usize, at);
            // SAFETY: releases exactly the packet taken above.
            unsafe { self.capture.ReleaseBuffer(frames) }?;
        }
    }

    /// The `Instant` of a QPC position in 100 ns units.
    fn instant_of(&self, position_100ns: u64) -> Instant {
        let mut counter = 0i64;
        // SAFETY: writes one integer.
        if unsafe { QueryPerformanceCounter(&mut counter) }.is_err() {
            return Instant::now();
        }
        let now = Instant::now();
        let now_100ns =
            (counter.max(0) as u128 * 10_000_000 / u128::from(self.qpc_frequency)) as u64;
        let age = Duration::from_nanos(now_100ns.saturating_sub(position_100ns) * 100);
        if age > MAX_AGE {
            return now;
        }
        now.checked_sub(age).unwrap_or(now)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stopping a started stream; an error on a lost device does not matter here.
        let _ = unsafe { self.client.Stop() };
    }
}
