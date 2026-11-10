//! Captured samples into 10 ms Opus packets with steady timestamps.
//!
//! WASAPI hands packets of any length. They are cut into 480-frame Opus frames. Each frame's time follows the one
//! before it by exactly 10 ms, so RTP timestamps step by exactly 480. When capture times jump away from that chain
//! (silence with no packets, a lost device), the chain starts again from the capture time.
//!
//! Silent frames are sent too: with DTX Opus makes them one byte after 200 ms. A gap in the stream makes the browser
//! fade the next sound in, and a click after a second of nothing lost its start (P9-1, D-038). When nothing plays,
//! Windows hands no packets at all; [`OpusPacketizer::fill_silence`] then keeps the chain going with silent frames.

use std::time::{Duration, Instant};

use opus::{Application, Bitrate, Channels, Encoder};

use crate::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

const FRAME: Duration = Duration::from_millis(10);
/// Capture times this far from the chain start it again.
const RESYNC: Duration = Duration::from_millis(20);
/// Silence fills the chain up to this long before now: captured sound reaches us 10-20 ms after it passed the
/// mixer and must still find the chain behind it.
const FILL_BEHIND: Duration = Duration::from_millis(40);
/// The largest Opus packet is 1275 bytes.
const MAX_PACKET: usize = 1500;

pub struct OpusPacketizer {
    encoder: Encoder,
    /// Interleaved samples of the frame being filled.
    pending: Vec<f32>,
    /// Capture time of the first sample in `pending`.
    pending_at: Instant,
    /// Where the next frame is expected on the chain.
    next_at: Option<Instant>,
    out: Vec<u8>,
    /// Frames that held nothing but zeros.
    pub silent_frames: u64,
}

fn sample_time(frames: usize) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE))
}

impl OpusPacketizer {
    pub fn new(bitrate_bps: i32) -> Result<Self, opus::Error> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate_bps))?;
        // DTX: 200 ms after a sound, silence shrinks to one-byte frames instead of the full bitrate.
        encoder.set_dtx(true)?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS),
            pending_at: Instant::now(),
            next_at: None,
            out: vec![0; MAX_PACKET],
            silent_frames: 0,
        })
    }

    /// Adds `frames` captured frames that started at `at`: interleaved stereo, or `None` for silence. `emit` gets
    /// every full 10 ms frame with its time on the chain.
    pub fn push(
        &mut self,
        samples: Option<&[f32]>,
        frames: usize,
        at: Instant,
        mut emit: impl FnMut(&[u8], Instant),
    ) -> Result<(), opus::Error> {
        let mut done = 0;
        while done < frames {
            if self.pending.is_empty() {
                self.pending_at = at + sample_time(done);
            }
            let room = FRAME_SAMPLES - self.pending.len() / CHANNELS;
            let take = room.min(frames - done);
            match samples {
                Some(s) => self
                    .pending
                    .extend_from_slice(&s[done * CHANNELS..(done + take) * CHANNELS]),
                None => self
                    .pending
                    .resize(self.pending.len() + take * CHANNELS, 0.0),
            }
            done += take;
            if self.pending.len() == FRAME_SAMPLES * CHANNELS {
                self.finish(&mut emit)?;
            }
        }
        Ok(())
    }

    /// Silent frames for the time nothing was captured. Returns how many were made.
    pub fn fill_silence(
        &mut self,
        now: Instant,
        mut emit: impl FnMut(&[u8], Instant),
    ) -> Result<u32, opus::Error> {
        let mut made = 0;
        // Nothing captured since the device opened: the chain starts here.
        if self.next_at.is_none() && self.pending.is_empty() {
            self.next_at = now.checked_sub(FILL_BEHIND);
        }
        while self.pending.is_empty()
            && let Some(next) = self.next_at
            && next + FILL_BEHIND <= now
        {
            self.pending.resize(FRAME_SAMPLES * CHANNELS, 0.0);
            self.pending_at = next;
            self.finish(&mut emit)?;
            made += 1;
        }
        Ok(made)
    }

    fn finish(&mut self, emit: &mut impl FnMut(&[u8], Instant)) -> Result<(), opus::Error> {
        let at = match self.next_at {
            Some(next) if next.max(self.pending_at) - next.min(self.pending_at) <= RESYNC => next,
            _ => self.pending_at,
        };
        self.next_at = Some(at + FRAME);
        if self.pending.iter().all(|s| *s == 0.0) {
            self.silent_frames += 1;
        }
        let result = self
            .encoder
            .encode_float(&self.pending, &mut self.out)
            .map(|n| emit(&self.out[..n], at));
        self.pending.clear();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, start: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| {
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn odd_packets_become_10_ms_frames_on_a_steady_chain() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        // 441-frame packets with a little jitter in their capture times, as WASAPI hands them.
        for k in 0..20usize {
            let jitter = Duration::from_micros((k as u64 % 3) * 300);
            let at = t0 + sample_time(k * 441) + jitter;
            p.push(Some(&tone(441, k * 441)), 441, at, |d, at| {
                out.push((d.len(), at))
            })
            .unwrap();
        }
        assert_eq!(out.len(), 20 * 441 / 480);
        assert_eq!(out[0].1, t0);
        for (i, (len, at)) in out.iter().enumerate() {
            assert!(*len > 10 && *len <= 1275, "packet {i} is {len} bytes");
            assert_eq!(*at, t0 + FRAME * i as u32);
        }
    }

    #[test]
    fn silence_goes_out_small_and_on_the_chain() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        p.push(Some(&tone(480, 0)), 480, t0, |d, at| {
            out.push((d.len(), at))
        })
        .unwrap();
        p.push(None, 480 * 30, t0 + FRAME, |d, at| out.push((d.len(), at)))
            .unwrap();
        let times: Vec<Instant> = out.iter().map(|o| o.1).collect();
        assert_eq!(times, (0..31).map(|i| t0 + FRAME * i).collect::<Vec<_>>());
        assert_eq!(p.silent_frames, 30);
        // DTX settles within 200 ms; then silence costs a byte a frame.
        assert!(out[22..].iter().all(|o| o.0 <= 3), "{out:?}");
    }

    #[test]
    fn silence_fills_a_quiet_device_behind_now_and_sound_joins_the_chain() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut keep = |_: &[u8], at: Instant| out.push(at);
        // Nothing captured since the device opened: the chain starts 40 ms before now.
        assert_eq!(p.fill_silence(t0 + FRAME * 10, &mut keep).unwrap(), 1);
        // 100 ms later, still nothing: frames up to 40 ms before now.
        assert_eq!(p.fill_silence(t0 + FRAME * 20, &mut keep).unwrap(), 10);
        // Sound captured 15 ms before now joins the chain where the silence ended.
        p.push(
            Some(&tone(480, 0)),
            480,
            t0 + FRAME * 20 - Duration::from_millis(15),
            &mut keep,
        )
        .unwrap();
        assert_eq!(out, (6..18).map(|i| t0 + FRAME * i).collect::<Vec<_>>());
    }

    #[test]
    fn a_gap_in_capture_restarts_the_chain() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        p.push(Some(&tone(480, 0)), 480, t0, |_, at| out.push(at))
            .unwrap();
        let later = t0 + Duration::from_millis(500);
        p.push(Some(&tone(480, 0)), 480, later, |_, at| out.push(at))
            .unwrap();
        assert_eq!(out, vec![t0, later]);
    }

    #[test]
    fn a_tone_comes_back_out_of_the_decoder() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Stereo).unwrap();
        let t0 = Instant::now();
        let mut energy = 0.0f32;
        let mut decoded = vec![0.0f32; FRAME_SAMPLES * CHANNELS];
        for k in 0..20 {
            let mut packets = Vec::new();
            p.push(
                Some(&tone(480, k * 480)),
                480,
                t0 + FRAME * k as u32,
                |d, _| packets.push(d.to_vec()),
            )
            .unwrap();
            for packet in packets {
                let n = decoder.decode_float(&packet, &mut decoded, false).unwrap();
                assert_eq!(n, FRAME_SAMPLES);
                if k > 5 {
                    energy += decoded.iter().map(|v| v * v).sum::<f32>();
                }
            }
        }
        // A 0.5 sine has mean square 0.125: 14 frames x 960 samples.
        let mean = energy / (14.0 * 960.0);
        assert!((0.09..0.16).contains(&mean), "mean square {mean}");
    }
}
