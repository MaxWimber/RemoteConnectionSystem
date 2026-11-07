//! Where a frame's time goes on the server (P8-5): from capture to the moment the network thread hands it to `str0m`,
//! and from capture to the moment its last packet leaves the pacer. The second minus the first is the pacer.
//!
//! The last packet of a frame carries the RTP marker bit and the frame's RTP timestamp. SRTP leaves the RTP header in
//! the clear, so the network thread reads both from the datagram it is about to send, without asking `str0m`.

use std::time::Duration;

/// Frames kept between two summaries: 10 s at 60 fps with room to spare. Storage is set aside once per session.
const CAPACITY: usize = 1024;

/// The RTP timestamp of a datagram that ends a video frame: RTP version 2, payload type `pt`, marker bit set.
/// RTCP and retransmissions (their own payload type) do not match.
pub fn frame_end(datagram: &[u8], pt: u8) -> Option<u32> {
    // RFC 5761: a second byte of 192–223 is RTCP on a shared port, whatever the payload type would read.
    if datagram.len() < 12 || datagram[0] >> 6 != 2 || (192..=223).contains(&datagram[1]) {
        return None;
    }
    let (marker, payload_type) = (datagram[1] & 0x80 != 0, datagram[1] & 0x7f);
    if !marker || payload_type != pt {
        return None;
    }
    Some(u32::from_be_bytes([
        datagram[4],
        datagram[5],
        datagram[6],
        datagram[7],
    ]))
}

/// How long ago a frame with RTP timestamp `ts` was captured, `since_epoch` after RTP time 0 (90 kHz, D-020).
/// The timestamp wraps every 13 hours; the frame is taken to be the most recent one with these low 32 bits.
pub fn age(ts: u32, since_epoch: Duration) -> Duration {
    let now_ticks = (since_epoch.as_micros() * 9 / 100) as u64;
    let low = now_ticks as u32;
    let back = u64::from(low.wrapping_sub(ts));
    // A timestamp a little ahead of the clock (rounding) counts as no time at all.
    if back > u64::from(u32::MAX / 2) {
        return Duration::ZERO;
    }
    Duration::from_micros(back * 100 / 9)
}

/// p50, p95, p99 and the maximum of one span, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

#[derive(Debug)]
pub struct FrameTimings {
    written: Vec<u32>,
    out: Vec<u32>,
}

impl Default for FrameTimings {
    fn default() -> Self {
        Self {
            written: Vec::with_capacity(CAPACITY),
            out: Vec::with_capacity(CAPACITY),
        }
    }
}

impl FrameTimings {
    /// Capture to the frame being written into `str0m`.
    pub fn written(&mut self, span: Duration) {
        push(&mut self.written, span);
    }

    /// Capture to the frame's last packet leaving the pacer.
    pub fn out(&mut self, span: Duration) {
        push(&mut self.out, span);
    }

    /// Both spans since the last call, then starts over. `None` for a span without frames.
    pub fn take(&mut self) -> (Option<Spread>, Option<Spread>) {
        let spans = (spread(&mut self.written), spread(&mut self.out));
        self.written.clear();
        self.out.clear();
        spans
    }
}

fn push(list: &mut Vec<u32>, span: Duration) {
    if list.len() < CAPACITY {
        list.push(u32::try_from(span.as_micros()).unwrap_or(u32::MAX));
    }
}

fn spread(list: &mut [u32]) -> Option<Spread> {
    if list.is_empty() {
        return None;
    }
    list.sort_unstable();
    let at = |p: f64| {
        let i = ((list.len() as f64 * p) as usize).min(list.len() - 1);
        (f64::from(list[i]) / 100.0).round() / 10.0
    };
    Some(Spread {
        p50: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: at(1.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(marker: bool, pt: u8, ts: u32) -> Vec<u8> {
        let mut d = vec![0x80, u8::from(marker) << 7 | pt, 0, 1];
        d.extend_from_slice(&ts.to_be_bytes());
        d.extend_from_slice(&[0; 8]);
        d
    }

    #[test]
    fn only_the_last_packet_of_a_video_frame_counts() {
        assert_eq!(frame_end(&rtp(true, 96, 1234), 96), Some(1234));
        assert_eq!(frame_end(&rtp(false, 96, 1234), 96), None);
        // A retransmission of the same packet under its own payload type.
        assert_eq!(frame_end(&rtp(true, 97, 1234), 96), None);
        // RTCP sender report: 0x80, 200.
        assert_eq!(
            frame_end(&[0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0], 72),
            None
        );
        // DTLS and STUN do not start with version 2.
        assert_eq!(
            frame_end(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0], 96),
            None
        );
        assert_eq!(frame_end(&[0x80, 0xe0], 96), None);
    }

    #[test]
    fn age_follows_the_90_khz_clock_across_the_wrap() {
        let since = Duration::from_millis(10_000);
        let now_ts = (10_000u64 * 90) as u32;
        assert_eq!(age(now_ts - 900, since), Duration::from_millis(10));
        assert_eq!(age(now_ts + 9, since), Duration::ZERO);
        // 13.3 hours in: the 32-bit timestamp has wrapped, the age is still 12 ms.
        let since = Duration::from_secs(48_000);
        let low = ((48_000u64 * 90_000) as u32).wrapping_sub(1080);
        assert_eq!(age(low, since), Duration::from_millis(12));
    }

    #[test]
    fn spreads_are_read_in_milliseconds_and_start_over() {
        let mut t = FrameTimings::default();
        for ms in 1..=100u64 {
            t.written(Duration::from_millis(ms));
            t.out(Duration::from_micros(ms * 1500));
        }
        let (written, out) = t.take();
        assert_eq!(
            written,
            Some(Spread {
                p50: 51.0,
                p95: 96.0,
                p99: 100.0,
                max: 100.0
            })
        );
        assert_eq!(out.map(|s| s.p50), Some(76.5));
        assert_eq!(t.take(), (None, None));
    }
}
