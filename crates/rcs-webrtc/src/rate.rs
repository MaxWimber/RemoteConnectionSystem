//! The encoder's target bitrate from the send-side estimate of `str0m` and the receiver reports (D-027).
//!
//! The estimate already follows queueing delay and loss. This layer keeps the encoder inside the range of the stream
//! mode, leaves headroom for retransmissions and padding, drops at once and climbs slowly, so the picture does not
//! pump with every new estimate.
//!
//! The estimate never strays far above what is actually sent: `str0m` keeps it 10-50% over the acknowledged rate.
//! A desktop sends a few Mbit/s, LCVBR stays under its target, so the estimate sinks with what goes out, and following
//! it down starves the encoder and sends even less (P8-1: a 40 Mbit/s link went down to 2 Mbit/s with no loss and an
//! RTT of 1.4 ms). So:
//!
//! - The target drops only on congestion: the estimate under what goes out (`str0m` sets it to 0.85 of the
//!   acknowledged rate on overuse) or heavy loss. An estimate under the target alone is no sign: LCVBR sends about
//!   70% of its target, so the estimate sits under it on a free link too (P8-1, second run).
//! - The estimate cannot tell how much more the link would carry, so with no sign of congestion the target climbs on
//!   its own; LCVBR only spends what the picture needs. For a while after a congestion drop it climbs only to a little
//!   above the rate the link carried then.

use std::time::{Duration, Instant};

/// Share of the estimate given to the encoder: the rest is for NACK retransmissions, RTCP and data channels.
const SHARE: f64 = 0.85;
/// The most the target grows per step, and how often it may.
const UP_STEP: f64 = 1.08;
const UP_EVERY: Duration = Duration::from_secs(1);
/// No growth for this long after the target dropped.
const HOLD_AFTER_DROP: Duration = Duration::from_secs(2);
/// Loss in receiver reports above this cuts the target; between `LOSS_HOLD` and it, the target holds.
const LOSS_CUT: f32 = 0.10;
const LOSS_HOLD: f32 = 0.02;
const LOSS_CUT_FACTOR: f64 = 0.8;
/// RTT this much above the lowest seen holds the target: a queue is building somewhere.
const RTT_RISE: Duration = Duration::from_millis(80);
/// An estimate under this share of the rate sent means the link no longer carries what goes out.
const UNDER_SENT: f64 = 0.9;
/// After a congestion drop, an application-limited target climbs to this many times the rate carried then...
const CEILING_ROOM: f64 = 1.5;
/// ...for this long.
const CEILING_FOR: Duration = Duration::from_secs(15);
/// The first estimates follow the start-up probes and the first IDR; only loss counts during this time.
const SETTLE: Duration = Duration::from_secs(2);
/// The rate sent is counted over two buckets of this length.
const BUCKET: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    pub start_bps: u32,
    pub min_bps: u32,
    pub max_bps: u32,
}

impl RateLimits {
    fn clamp(&self, bps: f64) -> u32 {
        bps.clamp(f64::from(self.min_bps), f64::from(self.max_bps)) as u32
    }
}

#[derive(Debug, Clone)]
pub struct RateControl {
    limits: RateLimits,
    target: u32,
    estimate: Option<u32>,
    last_up: Option<Instant>,
    last_drop: Option<Instant>,
    loss: Option<f32>,
    rtt: Option<Duration>,
    min_rtt: Option<Duration>,
    /// Bytes sent: the bucket that started at `.0`, and the whole one before it.
    sent: (Instant, u64, u64),
    first_estimate: Option<Instant>,
    /// The rate the link carried at the last congestion drop, and when.
    ceiling: Option<(f64, Instant)>,
}

impl RateControl {
    pub fn new(limits: RateLimits) -> Self {
        Self {
            target: limits.clamp(f64::from(limits.start_bps)),
            limits,
            estimate: None,
            last_up: None,
            last_drop: None,
            loss: None,
            rtt: None,
            min_rtt: None,
            sent: (Instant::now(), 0, 0),
            first_estimate: None,
            ceiling: None,
        }
    }

    /// Media bytes just handed to the session.
    pub fn on_sent(&mut self, now: Instant, bytes: usize) {
        self.roll(now);
        self.sent.1 += bytes as u64;
    }

    /// The rate sent over about the last second.
    pub fn sent_bps(&mut self, now: Instant) -> f64 {
        self.roll(now);
        let (start, current, previous) = self.sent;
        let span = BUCKET + now.duration_since(start);
        (current + previous) as f64 * 8.0 / span.as_secs_f64()
    }

    fn roll(&mut self, now: Instant) {
        let (start, current, _) = self.sent;
        let gone = now.duration_since(start);
        if gone >= BUCKET * 2 {
            self.sent = (now, 0, 0);
        } else if gone >= BUCKET {
            self.sent = (start + BUCKET, 0, current);
        }
    }

    pub fn limits(&self) -> RateLimits {
        self.limits
    }

    /// A new range, from the stream mode or the client settings. The target moves into it at once.
    pub fn set_limits(&mut self, limits: RateLimits) {
        self.limits = limits;
        self.target = limits.clamp(f64::from(self.target));
    }

    /// The bitrate the encoder should use now.
    pub fn target(&self) -> u32 {
        self.target
    }

    pub fn estimate(&self) -> Option<u32> {
        self.estimate
    }

    pub fn loss(&self) -> Option<f32> {
        self.loss
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    /// The send-side estimate of `str0m`.
    pub fn on_estimate(&mut self, now: Instant, estimate_bps: u64) {
        self.estimate = Some(estimate_bps.min(u64::from(u32::MAX)) as u32);
        let first = *self.first_estimate.get_or_insert(now);
        let estimate = estimate_bps as f64;
        let current = f64::from(self.target);
        let settled = now.duration_since(first) >= SETTLE;
        if settled && estimate < self.sent_bps(now) * UNDER_SENT {
            self.lower(now, estimate * SHARE, estimate);
            return;
        }
        if !self.may_rise(now) {
            return;
        }
        let room = self
            .ceiling
            .filter(|(_, at)| now.duration_since(*at) < CEILING_FOR)
            .map_or(f64::INFINITY, |(carried, _)| carried * CEILING_ROOM);
        let next = self.limits.clamp((current * UP_STEP).min(room));
        if next > self.target {
            self.target = next;
            self.last_up = Some(now);
        }
    }

    /// Loss and RTT from the receiver reports, as `str0m` sums them up about once a second.
    pub fn on_report(&mut self, now: Instant, loss: Option<f32>, rtt: Option<Duration>) {
        if let Some(rtt) = rtt {
            self.rtt = Some(rtt);
            self.min_rtt = Some(self.min_rtt.map_or(rtt, |m| m.min(rtt)));
        }
        if loss.is_some() {
            self.loss = loss;
        }
        if loss.is_some_and(|l| l > LOSS_CUT) {
            let next = f64::from(self.target) * LOSS_CUT_FACTOR;
            self.lower(now, next, next);
        }
    }

    /// Lowers the target after congestion; `carried` is what the link took then.
    fn lower(&mut self, now: Instant, bps: f64, carried: f64) {
        let next = self.limits.clamp(bps);
        if next < self.target {
            self.target = next;
            self.last_drop = Some(now);
            self.ceiling = Some((carried.max(f64::from(self.limits.min_bps)), now));
        }
    }

    fn may_rise(&self, now: Instant) -> bool {
        let since = |t: Option<Instant>, d: Duration| t.is_none_or(|t| now.duration_since(t) >= d);
        let queue = matches!((self.rtt, self.min_rtt), (Some(r), Some(m)) if r > m + RTT_RISE);
        since(self.last_up, UP_EVERY)
            && since(self.last_drop, HOLD_AFTER_DROP)
            && !self.loss.is_some_and(|l| l > LOSS_HOLD)
            && !queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: u32 = 1_000_000;

    fn control() -> (RateControl, Instant) {
        let limits = RateLimits {
            start_bps: 30 * MBPS,
            min_bps: 8 * MBPS,
            max_bps: 50 * MBPS,
        };
        (RateControl::new(limits), Instant::now())
    }

    /// A control past its start-up time at `t`.
    fn settled() -> (RateControl, Instant) {
        let (mut c, t) = control();
        let t = t + Duration::from_secs(5);
        c.first_estimate = Some(t - SETTLE);
        (c, t)
    }

    fn mbps(v: u32) -> u64 {
        u64::from(v) * u64::from(MBPS)
    }

    /// Sends at `rate` Mbit/s for the second before `at`.
    fn busy(c: &mut RateControl, at: Instant, rate: u32) {
        for ms in (0..1000).step_by(100) {
            let t = at - Duration::from_millis(1000 - ms);
            c.on_sent(t, (rate as usize * 1_000_000 / 8) / 10);
        }
    }

    #[test]
    fn drops_at_once_and_stays_in_range() {
        let (mut c, t) = settled();
        busy(&mut c, t, 30);
        c.on_estimate(t, mbps(20));
        assert_eq!(c.target(), 17 * MBPS);
        c.on_estimate(t, mbps(2));
        assert_eq!(c.target(), 8 * MBPS, "never under the mode minimum");
        assert_eq!(c.estimate(), Some(2 * MBPS));
    }

    #[test]
    fn the_start_up_estimates_do_not_count() {
        let (mut c, t) = control();
        busy(&mut c, t + Duration::from_secs(1), 30);
        c.on_estimate(t + Duration::from_secs(1), mbps(5));
        assert!(c.target() >= 30 * MBPS);
    }

    #[test]
    fn a_quiet_picture_does_not_drag_the_target_down() {
        let (mut c, t) = settled();
        busy(&mut c, t, 4);
        c.on_estimate(t, mbps(6));
        assert!(
            c.target() >= 30 * MBPS,
            "the estimate only follows the 4 Mbit/s sent"
        );
        // The link really narrows: the estimate falls under what goes out.
        c.on_estimate(t, mbps(3));
        assert_eq!(c.target(), 8 * MBPS);
    }

    #[test]
    fn after_congestion_a_quiet_picture_climbs_only_a_little_for_a_while() {
        let (mut c, t) = settled();
        busy(&mut c, t, 10);
        c.on_estimate(t, mbps(6));
        assert_eq!(c.target(), 8 * MBPS);
        // Quiet: 2 Mbit/s sent, no estimate above it. The ceiling is 1.5 x 8 Mbit/s.
        let mut at = t;
        for _ in 0..12 {
            at += UP_EVERY;
            busy(&mut c, at, 2);
            c.on_estimate(at, mbps(3));
        }
        assert_eq!(c.target(), 12 * MBPS);
        for _ in 0..40 {
            at += UP_EVERY;
            busy(&mut c, at, 2);
            c.on_estimate(at, mbps(3));
        }
        assert_eq!(c.target(), 50 * MBPS, "the ceiling expired");
    }

    #[test]
    fn an_estimate_under_the_target_but_over_what_goes_out_is_no_congestion() {
        let (mut c, t) = settled();
        // LCVBR sends 21 of 30 Mbit/s, the estimate follows it at 25.
        busy(&mut c, t, 21);
        c.on_estimate(t, mbps(25));
        assert!(c.target() >= 30 * MBPS);
    }

    #[test]
    fn the_rate_sent_counts_the_last_second() {
        let (mut c, t) = control();
        let t = t + Duration::from_secs(5);
        busy(&mut c, t, 16);
        let r = c.sent_bps(t);
        assert!((14e6..18e6).contains(&r), "{r}");
        assert_eq!(c.sent_bps(t + Duration::from_secs(3)), 0.0);
    }

    #[test]
    fn an_estimate_a_little_under_what_goes_out_is_jitter() {
        let (mut c, t) = settled();
        busy(&mut c, t, 30);
        c.on_estimate(t, mbps(28));
        assert!(c.target() >= 30 * MBPS);
    }

    #[test]
    fn climbs_slowly_and_not_right_after_a_drop() {
        let (mut c, t) = settled();
        busy(&mut c, t, 30);
        c.on_estimate(t, mbps(12));
        let low = c.target();
        c.on_estimate(t + Duration::from_millis(1500), mbps(100));
        assert_eq!(c.target(), low, "held after the drop");
        let mut at = t + HOLD_AFTER_DROP;
        c.on_estimate(at, mbps(100));
        assert_eq!(c.target(), (f64::from(low) * UP_STEP) as u32);
        let once = c.target();
        c.on_estimate(at + Duration::from_millis(500), mbps(100));
        assert_eq!(c.target(), once, "one step per second");
        for _ in 0..60 {
            at += UP_EVERY;
            c.on_estimate(at, mbps(100));
        }
        assert_eq!(c.target(), 50 * MBPS, "up to the mode maximum");
    }

    #[test]
    fn heavy_loss_cuts_and_some_loss_holds() {
        let (mut c, t) = control();
        c.on_report(t, Some(0.2), Some(Duration::from_millis(20)));
        assert_eq!(c.target(), 24 * MBPS);
        let at = t + Duration::from_secs(10);
        c.on_report(at, Some(0.05), None);
        c.on_estimate(at, mbps(100));
        assert_eq!(c.target(), 24 * MBPS, "5% loss holds");
        c.on_report(at, Some(0.0), None);
        c.on_estimate(at, mbps(100));
        assert!(c.target() > 24 * MBPS);
    }

    #[test]
    fn a_rising_rtt_holds() {
        let (mut c, t) = control();
        c.on_report(t, Some(0.0), Some(Duration::from_millis(30)));
        c.on_report(t, None, Some(Duration::from_millis(150)));
        c.on_estimate(t, mbps(100));
        assert_eq!(c.target(), 30 * MBPS);
        c.on_report(t, None, Some(Duration::from_millis(40)));
        c.on_estimate(t, mbps(100));
        assert_eq!(c.target(), (30.0 * f64::from(MBPS) * UP_STEP) as u32);
    }

    #[test]
    fn new_limits_move_the_target() {
        let (mut c, _) = control();
        c.set_limits(RateLimits {
            start_bps: 10 * MBPS,
            min_bps: 8 * MBPS,
            max_bps: 12 * MBPS,
        });
        assert_eq!(c.target(), 12 * MBPS);
    }
}
