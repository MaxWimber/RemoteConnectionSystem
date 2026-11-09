//! The encoder's target bitrate from the send-side estimate of `str0m` and the receiver reports (D-027).
//!
//! The estimate already follows queueing delay and loss. This layer keeps the encoder inside the range of the stream
//! mode, leaves headroom for retransmissions and padding, drops at once and climbs slowly, so the picture does not
//! pump with every new estimate.
//!
//! Without probes the estimate does not stray far above what is actually sent: `str0m` caps it at 1.5 times the
//! acknowledged rate. A desktop sends a few Mbit/s, LCVBR stays under its target, so following the estimate down
//! starves the encoder and sends even less. So (D-027, both addenda):
//!
//! - The target drops on congestion only:
//!   - `str0m` lowered its estimate (on overuse to 0.85 of the acknowledged rate) and it is now under what goes out;
//!   - the RTT grew by `RTT_RISE` while more goes out than the estimate, before `str0m` lowered it, or while the
//!     target is above the estimate: on a narrow link LCVBR peaks and IDR frames over it sit in its queue (P8-5);
//!   - heavy loss while the encoder sends at least half its target: down to what still gets through. Loss on a quiet
//!     picture only holds the target: a mobile network loses a few percent at any rate, one report of 0.4 Mbit/s
//!     with 22% lost cut 30 Mbit/s to the minimum and the picture to 720p (D-036). A link that really narrows under
//!     a quiet picture shows in the estimate and the RTT.
//!
//!   An estimate under the target alone is no sign: LCVBR sends about 70% of its target. Nor is an estimate that
//!   stays where it was while more goes out: after a decrease `str0m` climbs by a packet or so per step.
//! - With no sign of congestion the target climbs on a clock, not only when a new estimate comes (`str0m` reports
//!   one only when it changes), and goes straight to its share of a higher estimate, which probes measure. An encoder
//!   that spends its whole target (`mf`, a busy picture) climbs only with the estimate; one that leaves part unused
//!   climbs past it, LCVBR only spends what the picture needs. For a while after a congestion drop the target climbs
//!   only to a little above the rate the link carried then.

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
/// Loss cuts only while at least this share of the target goes out: then the link may not carry what is sent.
const LOSS_BUSY: f64 = 0.5;
/// RTT this much above the lowest seen holds the target: a queue is building somewhere.
const RTT_RISE: Duration = Duration::from_millis(80);
/// An estimate under this share of the rate sent means the link no longer carries what goes out.
const UNDER_SENT: f64 = 0.9;
/// An estimate under this share of the one before is a decrease by `str0m`, not rounding.
const FELL: f64 = 0.99;
/// Sending this share of the target or more: the encoder spends what it is given.
const SPENDS: f64 = 0.9;
/// After a congestion drop, an application-limited target climbs to this many times the rate carried then...
const CEILING_ROOM: f64 = 1.2;
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
        let before = self
            .estimate
            .replace(estimate_bps.min(u64::from(u32::MAX)) as u32);
        let first = *self.first_estimate.get_or_insert(now);
        let estimate = estimate_bps as f64;
        let settled = now.duration_since(first) >= SETTLE;
        let fell = before.is_none_or(|b| estimate < f64::from(b) * FELL);
        if settled && fell && estimate < self.sent_bps(now) * UNDER_SENT {
            self.lower(now, estimate * SHARE, estimate);
            return;
        }
        self.rise(now);
    }

    /// Lets the target climb with no new estimate; call it often, it steps at most once per `UP_EVERY`.
    pub fn on_tick(&mut self, now: Instant) {
        if self
            .first_estimate
            .is_some_and(|first| now.duration_since(first) >= SETTLE)
        {
            self.rise(now);
        }
    }

    fn rise(&mut self, now: Instant) {
        if !self.may_rise(now) {
            return;
        }
        let current = f64::from(self.target);
        let room = self
            .ceiling
            .filter(|(_, at)| now.duration_since(*at) < CEILING_FOR)
            .map_or(f64::INFINITY, |(carried, _)| carried * CEILING_ROOM);
        // An encoder that spends its whole target (`mf`, or a busy picture) would send everything above the
        // estimate into the link; then the target climbs with the estimate, and `str0m` probes for more.
        let spends = self.sent_bps(now) >= current * SPENDS;
        let cap = match self.estimate {
            Some(estimate) if spends => f64::from(estimate),
            _ => f64::INFINITY,
        };
        let step = (current * UP_STEP).min(room).min(cap);
        // A higher estimate is what `str0m` measured, for example with a probe after the link came back: the target
        // may go straight to its share instead of climbing there.
        // Not past the ceiling after congestion: during a loss storm the estimate may still be the one from before.
        let follow = self
            .estimate
            .map_or(0.0, |e| f64::from(e) * SHARE)
            .min(room);
        let next = self.limits.clamp(step.max(follow));
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
        let sent = self.sent_bps(now);
        if let Some(loss) = loss.filter(|l| *l > LOSS_CUT)
            && sent >= f64::from(self.target) * LOSS_BUSY
        {
            // What reaches the browser is a better floor than a fixed share when most of it is lost.
            let delivered = sent * f64::from(1.0 - loss.min(1.0));
            let mut next = f64::from(self.target) * LOSS_CUT_FACTOR;
            if delivered > 0.0 {
                next = next.min(delivered);
            }
            self.lower(now, next, next);
            return;
        }
        // A queue builds while more goes out than the estimate, and `str0m` has not lowered it (yet). Or the target
        // climbed past the estimate while the encoder left part of it unused, and its peaks now fill a narrow link
        // (P8-5: 3 Mbit/s link, target 3.7-4.1, RTT up to 290 ms while on average less went out than the estimate).
        if self.queue_builds()
            && let Some(estimate) = self.estimate.map(f64::from)
            && (estimate < sent * UNDER_SENT || f64::from(self.target) > estimate)
        {
            self.lower(now, estimate * SHARE, estimate);
        }
    }

    fn queue_builds(&self) -> bool {
        matches!((self.rtt, self.min_rtt), (Some(r), Some(m)) if r > m + RTT_RISE)
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
        since(self.last_up, UP_EVERY)
            && since(self.last_drop, HOLD_AFTER_DROP)
            && !self.loss.is_some_and(|l| l > LOSS_HOLD)
            && !self.queue_builds()
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
        // Quiet: 2 Mbit/s sent, no estimate above it. The ceiling is 1.2 x 8 Mbit/s.
        let mut at = t;
        for _ in 0..12 {
            at += UP_EVERY;
            busy(&mut c, at, 2);
            c.on_estimate(at, mbps(3));
        }
        assert_eq!(c.target(), 9_600_000);
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
        c.on_estimate(t + Duration::from_millis(1500), mbps(12));
        assert_eq!(c.target(), low, "held after the drop");
        let mut at = t + HOLD_AFTER_DROP;
        c.on_estimate(at, mbps(12));
        assert_eq!(c.target(), (f64::from(low) * UP_STEP) as u32);
        let once = c.target();
        c.on_estimate(at + Duration::from_millis(500), mbps(12));
        assert_eq!(c.target(), once, "one step per second");
        for _ in 0..60 {
            at += UP_EVERY;
            c.on_estimate(at, mbps(12));
        }
        assert_eq!(c.target(), 50 * MBPS, "up to the mode maximum");
    }

    #[test]
    fn heavy_loss_cuts_and_some_loss_holds() {
        let (mut c, t) = control();
        busy(&mut c, t, 30);
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
        c.on_estimate(t, mbps(30));
        assert_eq!(c.target(), 30 * MBPS);
        c.on_report(t, None, Some(Duration::from_millis(40)));
        c.on_estimate(t, mbps(30));
        assert_eq!(c.target(), (30.0 * f64::from(MBPS) * UP_STEP) as u32);
    }

    #[test]
    fn the_target_climbs_on_the_clock_without_new_estimates() {
        let (mut c, t) = settled();
        c.estimate = Some(11 * MBPS);
        c.target = 11 * MBPS;
        let mut at = t;
        for _ in 0..5 {
            at += UP_EVERY;
            c.on_tick(at);
        }
        assert!(c.target() > 16 * MBPS, "{}", c.target());
        // Ticks closer than a second apart step once.
        let once = c.target();
        c.on_tick(at + Duration::from_millis(300));
        assert_eq!(c.target(), once);
    }

    #[test]
    fn no_climb_on_the_clock_before_the_first_estimates_settle() {
        let (mut c, t) = control();
        c.on_tick(t + Duration::from_secs(5));
        assert_eq!(c.target(), 30 * MBPS, "no estimate yet");
        c.on_estimate(t + Duration::from_secs(5), mbps(30));
        let first = c.target();
        c.on_tick(t + Duration::from_secs(6));
        assert_eq!(c.target(), first, "still settling");
        c.on_tick(t + Duration::from_secs(8));
        assert!(c.target() > first);
    }

    #[test]
    fn an_estimate_that_stays_while_more_goes_out_is_no_congestion() {
        // `mf` sends its whole target, which has climbed past an estimate that grows a packet at a time.
        let (mut c, t) = settled();
        busy(&mut c, t, 11);
        c.on_estimate(t, mbps(11));
        c.target = 14 * MBPS;
        busy(&mut c, t + Duration::from_secs(1), 14);
        c.on_estimate(t + Duration::from_secs(1), mbps(11));
        assert!(c.target() >= 14 * MBPS, "{}", c.target());
        // str0m lowers it: now it is congestion.
        busy(&mut c, t + Duration::from_secs(2), 14);
        c.on_estimate(t + Duration::from_secs(2), mbps(10));
        assert_eq!(c.target(), (10.0 * f64::from(MBPS) * SHARE) as u32);
    }

    #[test]
    fn a_lower_estimate_still_over_what_goes_out_is_no_congestion() {
        let (mut c, t) = settled();
        busy(&mut c, t, 6);
        c.on_estimate(t, mbps(30));
        c.on_estimate(t, mbps(20));
        assert!(c.target() >= 30 * MBPS);
    }

    #[test]
    fn a_growing_queue_drops_to_the_estimate_while_more_goes_out() {
        let (mut c, t) = settled();
        c.on_report(t, Some(0.0), Some(Duration::from_millis(2)));
        c.on_estimate(t, mbps(36));
        c.target = 50 * MBPS;
        busy(&mut c, t + Duration::from_secs(1), 50);
        c.on_report(
            t + Duration::from_secs(1),
            Some(0.0),
            Some(Duration::from_millis(120)),
        );
        assert_eq!(c.target(), (36.0 * f64::from(MBPS) * SHARE) as u32);
    }

    #[test]
    fn a_growing_queue_with_less_going_out_than_the_estimate_only_holds() {
        let (mut c, t) = settled();
        c.on_report(t, Some(0.0), Some(Duration::from_millis(2)));
        c.on_estimate(t, mbps(36));
        busy(&mut c, t + Duration::from_secs(1), 10);
        c.on_report(
            t + Duration::from_secs(1),
            Some(0.0),
            Some(Duration::from_millis(120)),
        );
        assert!(c.target() >= 30 * MBPS);
    }

    #[test]
    fn a_growing_queue_with_the_target_over_the_estimate_drops_to_it() {
        // A narrow link: LCVBR sends 2.4 Mbit/s on average, but the target has climbed to 3.7 over the estimate of
        // 3.2 and its peaks fill the link's queue.
        let (mut c, t) = settled();
        c.limits.min_bps = 2 * MBPS;
        c.on_report(t, Some(0.0), Some(Duration::from_millis(2)));
        c.on_estimate(t, mbps(3) + 200_000);
        c.target = 3_700_000;
        let at = t + Duration::from_secs(1);
        c.on_sent(at - Duration::from_millis(900), 150_000);
        c.on_sent(at - Duration::from_millis(400), 150_000);
        c.on_report(at, Some(0.0), Some(Duration::from_millis(197)));
        assert_eq!(c.target(), (3.2e6 * SHARE) as u32);
        // The same RTT with the target under the estimate only holds.
        let (mut c, t) = settled();
        c.on_report(t, Some(0.0), Some(Duration::from_millis(2)));
        c.on_estimate(t, mbps(40));
        let held = c.target();
        assert!(held < 40 * MBPS);
        c.on_report(
            t + Duration::from_secs(1),
            Some(0.0),
            Some(Duration::from_millis(197)),
        );
        assert_eq!(c.target(), held);
    }

    #[test]
    fn heavy_loss_cuts_to_what_gets_through() {
        let (mut c, t) = settled();
        busy(&mut c, t, 20);
        c.on_report(t, Some(0.5), None);
        let target = f64::from(c.target());
        assert!((9.5e6..10.5e6).contains(&target), "{target}");
    }

    #[test]
    fn loss_on_a_quiet_picture_holds_but_does_not_cut() {
        // From the phone on mobile internet (D-036): a still desktop sends 0.4 Mbit/s, one report says 22% lost,
        // the estimate stays at 31 Mbit/s.
        let (mut c, t) = settled();
        c.limits.min_bps = 2 * MBPS;
        c.on_estimate(t, mbps(31));
        let held = c.target();
        let at = t + Duration::from_secs(1);
        c.on_sent(at - Duration::from_millis(900), 25_000);
        c.on_sent(at - Duration::from_millis(400), 25_000);
        c.on_report(at, Some(0.225), Some(Duration::from_millis(27)));
        assert_eq!(c.target(), held, "no cut");
        c.on_tick(at + UP_EVERY);
        assert_eq!(c.target(), held, "but no climb either");
        // The same loss while the encoder spends most of its target is congestion.
        let at = at + Duration::from_secs(2);
        busy(&mut c, at, 20);
        c.on_report(at, Some(0.225), Some(Duration::from_millis(27)));
        assert!(c.target() < held, "{}", c.target());
    }

    #[test]
    fn an_encoder_that_spends_its_target_climbs_only_with_the_estimate() {
        let (mut c, t) = settled();
        c.target = 20 * MBPS;
        c.estimate = Some(21 * MBPS);
        busy(&mut c, t, 20);
        c.on_tick(t);
        assert_eq!(c.target(), 21 * MBPS);
        busy(&mut c, t + UP_EVERY, 21);
        c.on_tick(t + UP_EVERY);
        assert_eq!(c.target(), 21 * MBPS, "at the estimate");
        // LCVBR leaves most of it unused: the clock lifts it past the estimate.
        busy(&mut c, t + UP_EVERY * 2, 8);
        c.on_tick(t + UP_EVERY * 2);
        assert!(c.target() > 21 * MBPS);
    }

    #[test]
    fn a_probed_estimate_lifts_the_target_at_once() {
        let (mut c, t) = settled();
        busy(&mut c, t, 10);
        c.on_estimate(t, mbps(3));
        assert_eq!(c.target(), 8 * MBPS);
        // Right after the drop only up to the ceiling, 1.2 x 8 Mbit/s: the estimate may be a stale one.
        let at = t + HOLD_AFTER_DROP;
        busy(&mut c, at, 3);
        c.on_estimate(at, mbps(30));
        assert_eq!(c.target(), 9_600_000);
        let at = t + CEILING_FOR;
        busy(&mut c, at, 3);
        c.on_estimate(at, mbps(31));
        assert_eq!(c.target(), (31.0 * f64::from(MBPS) * SHARE) as u32);
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
