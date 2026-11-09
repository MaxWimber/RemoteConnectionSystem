//! Captured samples into 10 ms Opus packets with steady timestamps.
//!
//! WASAPI hands packets of any length. They are cut into 480-frame Opus frames. Each frame's time follows the one
//! before it by exactly 10 ms, so RTP timestamps step by exactly 480. When capture times jump away from that chain
//! (silence with no packets, a lost device), the chain starts again from the capture time.

use std::time::{Duration, Instant};

use opus::{Application, Bitrate, Channels, Encoder};

use crate::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

const FRAME: Duration = Duration::from_millis(10);
/// Capture times this far from the chain start it again.
const RESYNC: Duration = Duration::from_millis(20);
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
    /// Frames that held nothing but zeros and were not sent.
    pub silent_frames: u64,
}

fn sample_time(frames: usize) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE))
}

impl OpusPacketizer {
    pub fn new(bitrate_bps: i32) -> Result<Self, opus::Error> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate_bps))?;
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
    /// every full 10 ms frame that is not all zeros, with its time on the chain.
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

    fn finish(&mut self, emit: &mut impl FnMut(&[u8], Instant)) -> Result<(), opus::Error> {
        let at = match self.next_at {
            Some(next) if next.max(self.pending_at) - next.min(self.pending_at) <= RESYNC => next,
            _ => self.pending_at,
        };
        self.next_at = Some(at + FRAME);
        let result = if self.pending.iter().all(|s| *s == 0.0) {
            self.silent_frames += 1;
            Ok(())
        } else {
            self.encoder
                .encode_float(&self.pending, &mut self.out)
                .map(|n| emit(&self.out[..n], at))
        };
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
    fn zeros_are_not_sent_and_the_chain_goes_through_them() {
        let mut p = OpusPacketizer::new(128_000).unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        p.push(Some(&tone(480, 0)), 480, t0, |_, at| out.push(at))
            .unwrap();
        p.push(None, 960, t0 + FRAME, |_, at| out.push(at)).unwrap();
        p.push(Some(&tone(480, 0)), 480, t0 + FRAME * 3, |_, at| {
            out.push(at)
        })
        .unwrap();
        assert_eq!(out, vec![t0, t0 + FRAME * 3]);
        assert_eq!(p.silent_frames, 2);
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
