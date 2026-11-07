//! The encoder's target bitrate from the send-side estimate of `str0m` and the receiver reports (D-027).
//!
//! The estimate already follows queueing delay and loss. This layer only keeps the encoder inside the range of the
//! stream mode, leaves headroom for retransmissions and padding, drops at once and climbs slowly, so the picture
//! does not pump with every new estimate.

use std::time::{Duration, Instant};

/// Share of the estimate given to the encoder: the rest is for NACK retransmissions, RTCP and data channels.
const SHARE: f64 = 0.85;
/// A new estimate below the target by less than this does not lower it: estimates jitter a little.
const DOWN_DEADBAND: f64 = 0.05;
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
        let wanted = estimate_bps as f64 * SHARE;
        let current = f64::from(self.target);
        if wanted < current * (1.0 - DOWN_DEADBAND) {
            self.lower(now, wanted);
        } else if wanted > current && self.may_rise(now) {
            let next = self.limits.clamp(wanted.min(current * UP_STEP));
            if next > self.target {
                self.target = next;
                self.last_up = Some(now);
            }
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
            self.lower(now, f64::from(self.target) * LOSS_CUT_FACTOR);
        }
    }

    fn lower(&mut self, now: Instant, bps: f64) {
        let next = self.limits.clamp(bps);
        if next < self.target {
            self.target = next;
            self.last_drop = Some(now);
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

    fn mbps(v: u32) -> u64 {
        u64::from(v) * u64::from(MBPS)
    }

    #[test]
    fn drops_at_once_and_stays_in_range() {
        let (mut c, t) = control();
        c.on_estimate(t, mbps(20));
        assert_eq!(c.target(), 17 * MBPS);
        c.on_estimate(t, mbps(2));
        assert_eq!(c.target(), 8 * MBPS, "never under the mode minimum");
        assert_eq!(c.estimate(), Some(2 * MBPS));
    }

    #[test]
    fn small_dips_are_ignored() {
        let (mut c, t) = control();
        // 85% of 34 Mbit/s is 28.9, inside the dead band of 30.
        c.on_estimate(t, mbps(34));
        assert_eq!(c.target(), 30 * MBPS);
    }

    #[test]
    fn climbs_slowly_and_not_right_after_a_drop() {
        let (mut c, t) = control();
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
        for _ in 0..40 {
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
