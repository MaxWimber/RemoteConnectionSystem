//! The audio thread (D-038): loopback of the default output device into Opus packets for the transport.
//!
//! Capture is open only while a browser is connected. Once a second the thread checks the default device; when the
//! user switches it, when the device goes away, or when there is none, capture is closed and opened again later.
//! Video never waits for any of this. No network or disk work happens here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_audio::{AudioError, ComApartment, Loopback, OpusPacketizer, default_device_id};
use rcs_webrtc::{AudioPacket, FrameSink};
use tracing::{info, warn};

/// How often the buffer is read; WASAPI holds 100 ms.
const POLL: Duration = Duration::from_millis(5);
/// How often nobody-connected is checked.
const IDLE: Duration = Duration::from_millis(100);
/// How often the default device is compared with the open one, and a failed open is tried again.
const CHECK_EVERY: Duration = Duration::from_secs(1);
const BITRATE_BPS: i32 = 128_000;

struct Open {
    loopback: Loopback,
    packetizer: OpusPacketizer,
    opened: Instant,
}

pub fn spawn(sink: FrameSink, stop: Arc<AtomicBool>) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || run(&sink, &stop))
}

fn run(sink: &FrameSink, stop: &AtomicBool) {
    let _com = match ComApartment::enter() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "audio: COM unavailable, no sound");
            return;
        }
    };
    info!(opus = rcs_audio::opus_version(), "audio thread started");
    let mut open: Option<Open> = None;
    let mut next_check = Instant::now();
    // The last problem logged, so a missing device is reported once and not every second.
    let mut problem: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        if !sink.connected() {
            if let Some(o) = open.take() {
                close(o, "nobody connected");
            }
            std::thread::sleep(IDLE);
            continue;
        }
        let now = Instant::now();
        if open.is_none() && now >= next_check {
            next_check = now + CHECK_EVERY;
            match Loopback::open().and_then(|loopback| {
                let packetizer = OpusPacketizer::new(BITRATE_BPS)?;
                Ok((loopback, packetizer))
            }) {
                Ok((loopback, packetizer)) => {
                    info!(device = loopback.device_id(), "audio capture opened");
                    problem = None;
                    open = Some(Open {
                        loopback,
                        packetizer,
                        opened: now,
                    });
                }
                Err(e) => report(&mut problem, &e),
            }
        }
        if let Some(o) = open.as_mut() {
            let Open {
                loopback,
                packetizer,
                ..
            } = o;
            let mut encode_error = None;
            let read = loopback.read(|samples, frames, at| {
                let pushed = packetizer.push(samples, frames, at, |bytes, captured| {
                    let mut data = sink.audio_buffer();
                    data.extend_from_slice(bytes);
                    sink.push_audio(AudioPacket { data, captured });
                });
                if let Err(e) = pushed {
                    encode_error = Some(e);
                }
            });
            if encode_error.is_none()
                && let Err(e) = packetizer.fill_silence(Instant::now(), |bytes, captured| {
                    let mut data = sink.audio_buffer();
                    data.extend_from_slice(bytes);
                    sink.push_audio(AudioPacket { data, captured });
                })
            {
                encode_error = Some(e);
            }
            if let Some(e) = encode_error {
                warn!(error = %e, "opus encode failed");
            }
            let mut reopen = match read {
                Ok(()) => None,
                Err(e) => {
                    report(&mut problem, &e);
                    Some("capture failed")
                }
            };
            if reopen.is_none() && now >= next_check {
                next_check = now + CHECK_EVERY;
                reopen = match default_device_id() {
                    Ok(id) if id != o.loopback.device_id() => Some("default device changed"),
                    Ok(_) => None,
                    Err(e) => {
                        report(&mut problem, &e);
                        Some("default device gone")
                    }
                };
            }
            if let Some(why) = reopen
                && let Some(o) = open.take()
            {
                close(o, why);
                // Open the new device on the next pass, not a second later.
                next_check = Instant::now();
            }
        }
        std::thread::sleep(POLL);
    }
    if let Some(o) = open.take() {
        close(o, "agent stopping");
    }
}

fn close(open: Open, why: &str) {
    info!(
        why,
        seconds = open.opened.elapsed().as_secs(),
        silent_frames = open.packetizer.silent_frames,
        "audio capture closed"
    );
}

fn report(last: &mut Option<String>, error: &AudioError) {
    let text = error.to_string();
    if last.as_deref() != Some(text.as_str()) {
        warn!(error = %text, "audio capture unavailable");
        *last = Some(text);
    }
}
