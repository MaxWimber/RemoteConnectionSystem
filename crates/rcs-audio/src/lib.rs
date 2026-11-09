//! Sound of the default output device (D-038): WASAPI loopback after the Windows mixer, 48 kHz stereo, Opus in
//! 10 ms packets. Nothing here touches the network or the disk: the caller hands packets to the transport.

mod encode;
mod wasapi;

pub use encode::OpusPacketizer;
pub use wasapi::{AudioError, ComApartment, Loopback, default_device_id};

/// Samples per second per channel, the Opus and RTP clock.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved stereo.
pub const CHANNELS: usize = 2;
/// One Opus packet: 10 ms per channel.
pub const FRAME_SAMPLES: usize = 480;

/// The libopus version built in, for the log.
pub fn opus_version() -> &'static str {
    opus::version()
}
