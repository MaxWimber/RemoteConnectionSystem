//! Stream modes and when «Слабая сеть» comes on by itself (D-030).

use std::time::{Duration, Instant};

/// A stream mode from `HARDWARE.md`: the picture size and the top of the target range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Game,
    Desktop,
    Weak,
}

impl StreamMode {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "game" => Some(Self::Game),
            "desktop" => Some(Self::Desktop),
            "weak" => Some(Self::Weak),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Desktop => "desktop",
            Self::Weak => "weak",
        }
    }

    /// The top of the target range in this mode.
    pub fn max_bps(self) -> u32 {
        match self {
            Self::Game => 30_000_000,
            Self::Desktop => 40_000_000,
            Self::Weak => 12_000_000,
        }
    }

    /// The picture size for a full size of `full`: two thirds for «Слабая сеть», even for the encoder.
    pub fn size(self, full: (i32, i32)) -> (i32, i32) {
        match self {
            Self::Weak => ((full.0 * 2 / 3) & !1, (full.1 * 2 / 3) & !1),
            Self::Game | Self::Desktop => full,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Desktop,
            2 => Self::Weak,
            _ => Self::Game,
        }
    }
}

/// A target under this for `WEAK_AFTER` turns «Слабая сеть» on.
const WEAK_BELOW: u32 = 5_000_000;
const WEAK_AFTER: Duration = Duration::from_secs(5);
/// An estimate this high for `BACK_AFTER`, with loss up to `BACK_LOSS` on average over that time, turns it off
/// again. The loss is the one that still lets the target climb. On a mobile network 3-7% is always there and single
/// reports jump from 0 to 10%, so the average counts, not each report (D-047).
const BACK_ABOVE: u32 = 15_000_000;
const BACK_LOSS: f32 = crate::rate::LOSS_HOLD;
const BACK_AFTER: Duration = Duration::from_secs(10);
/// The least time between two automatic changes.
const MIN_STAY: Duration = Duration::from_secs(20);

/// The mode the client chose and the one streaming now.
#[derive(Debug, Clone)]
pub struct ModeControl {
    chosen: StreamMode,
    active: StreamMode,
    low_since: Option<Instant>,
    high_since: Option<Instant>,
    /// Loss reports since `high_since`: their sum and count.
    high_loss: (f32, u32),
    changed_at: Instant,
}

impl ModeControl {
    /// Starts in `mode` as if the client had chosen it; the client's own choice comes right after the connection.
    pub fn new(now: Instant, mode: StreamMode) -> Self {
        Self {
            chosen: mode,
            active: mode,
            low_since: None,
            high_since: None,
            high_loss: (0.0, 0),
            changed_at: now,
        }
    }

    pub fn chosen(&self) -> StreamMode {
        self.chosen
    }

    pub fn active(&self) -> StreamMode {
        self.active
    }

    /// The client's choice, which applies at once.
    pub fn choose(&mut self, now: Instant, mode: StreamMode) {
        self.chosen = mode;
        self.active = mode;
        self.low_since = None;
        self.high_since = None;
        self.changed_at = now;
    }

    /// Once a second with the rate control's state; `cap_bps` is the top of the chosen mode with the client's limit.
    /// Returns true when the active mode changed.
    pub fn on_rate(
        &mut self,
        now: Instant,
        target_bps: u32,
        estimate_bps: Option<u32>,
        loss: Option<f32>,
        cap_bps: u32,
    ) -> bool {
        if self.chosen == StreamMode::Weak {
            return false;
        }
        let settled = now.duration_since(self.changed_at) >= MIN_STAY;
        if self.active == self.chosen {
            self.high_since = None;
            if target_bps >= WEAK_BELOW {
                self.low_since = None;
                return false;
            }
            let since = *self.low_since.get_or_insert(now);
            if settled && now.duration_since(since) >= WEAK_AFTER {
                self.switch(now, StreamMode::Weak);
                return true;
            }
            return false;
        }
        // In «Слабая сеть» by itself: back once the link has room for the chosen mode again.
        self.low_since = None;
        let room = estimate_bps.unwrap_or(0).min(cap_bps);
        if room < BACK_ABOVE {
            self.high_since = None;
            return false;
        }
        if self.high_since.is_none() {
            self.high_since = Some(now);
            self.high_loss = (0.0, 0);
        }
        if let Some(l) = loss {
            self.high_loss.0 += l;
            self.high_loss.1 += 1;
        }
        let since = self.high_since.unwrap_or(now);
        if now.duration_since(since) < BACK_AFTER {
            return false;
        }
        let (sum, n) = self.high_loss;
        let mean = if n > 0 { sum / n as f32 } else { 0.0 };
        if mean > BACK_LOSS {
            // A lossy stretch: count again from here.
            self.high_since = Some(now);
            self.high_loss = (0.0, 0);
            return false;
        }
        if settled {
            self.switch(now, self.chosen);
            return true;
        }
        false
    }

    fn switch(&mut self, now: Instant, mode: StreamMode) {
        self.active = mode;
        self.low_since = None;
        self.high_since = None;
        self.changed_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: u32 = 1_000_000;

    /// Feeds the same state once a second for `secs` seconds from `at`; returns the time after and whether it switched.
    fn run(
        m: &mut ModeControl,
        at: Instant,
        secs: u64,
        target: u32,
        estimate: u32,
        loss: f32,
    ) -> (Instant, bool) {
        let mut switched = false;
        let mut t = at;
        for _ in 0..secs {
            t += Duration::from_secs(1);
            switched |= m.on_rate(
                t,
                target * MBPS,
                Some(estimate * MBPS),
                Some(loss),
                30 * MBPS,
            );
        }
        (t, switched)
    }

    #[test]
    fn sizes_and_names() {
        assert_eq!(StreamMode::Weak.size((1920, 1080)), (1280, 720));
        assert_eq!(StreamMode::Weak.size((2560, 1441)), (1706, 960));
        assert_eq!(StreamMode::Game.size((1920, 1080)), (1920, 1080));
        for m in [StreamMode::Game, StreamMode::Desktop, StreamMode::Weak] {
            assert_eq!(StreamMode::parse(m.name()), Some(m));
            assert_eq!(StreamMode::from_u8(m.to_u8()), m);
        }
        assert_eq!(StreamMode::parse("1440p"), None);
    }

    #[test]
    fn a_low_target_turns_weak_on_and_room_turns_it_off() {
        let t0 = Instant::now();
        let mut m = ModeControl::new(t0, StreamMode::Game);
        // Not in the first 20 s of a mode.
        let (t, switched) = run(&mut m, t0, 19, 2, 3, 0.1);
        assert!(!switched);
        let (t, switched) = run(&mut m, t, 2, 2, 3, 0.1);
        assert!(switched);
        assert_eq!(m.active(), StreamMode::Weak);
        assert_eq!(m.chosen(), StreamMode::Game);
        // Room again, but it must hold 10 s and the mode 20 s.
        let (t, switched) = run(&mut m, t, 15, 12, 20, 0.0);
        assert!(!switched, "stayed only 15 s");
        let (_, switched) = run(&mut m, t, 6, 12, 20, 0.0);
        assert!(switched);
        assert_eq!(m.active(), StreamMode::Game);
    }

    #[test]
    fn a_short_dip_or_lossy_room_changes_nothing() {
        let t0 = Instant::now();
        let mut m = ModeControl::new(t0, StreamMode::Game);
        let (t, _) = run(&mut m, t0, 30, 20, 25, 0.0);
        let (t, s1) = run(&mut m, t, 4, 2, 3, 0.0);
        let (t, s2) = run(&mut m, t, 1, 20, 25, 0.0);
        let (_, s3) = run(&mut m, t, 4, 2, 3, 0.0);
        assert!(!(s1 || s2 || s3), "never five seconds in a row");
        let mut m = ModeControl::new(t0, StreamMode::Game);
        let (t, _) = run(&mut m, t0, 30, 2, 3, 0.0);
        assert_eq!(m.active(), StreamMode::Weak);
        let (t, back) = run(&mut m, t, 40, 12, 20, 0.07);
        assert!(!back, "loss above 5%");
        let (_, back) = run(&mut m, t, 40, 12, 20, 0.04);
        assert!(back, "mobile loss of 4% lets 1080p back");
    }

    #[test]
    fn jumpy_mobile_loss_counts_on_average() {
        // From the phone (D-047): reports jump between 0 and 10% around 4%.
        let t0 = Instant::now();
        let mut m = ModeControl::new(t0, StreamMode::Game);
        let (mut t, _) = run(&mut m, t0, 30, 2, 3, 0.0);
        assert_eq!(m.active(), StreamMode::Weak);
        let losses = [
            0.022, 0.039, 0.025, 0.097, 0.036, 0.10, 0.0, 0.0, 0.027, 0.062, 0.0, 0.05,
        ];
        let mut back = false;
        for l in losses {
            t += Duration::from_secs(1);
            back |= m.on_rate(t, 12 * MBPS, Some(21 * MBPS), Some(l), 30 * MBPS);
        }
        assert!(back, "4% on average");
        assert_eq!(m.active(), StreamMode::Game);
    }

    #[test]
    fn a_low_client_limit_keeps_weak() {
        let t0 = Instant::now();
        let mut m = ModeControl::new(t0, StreamMode::Game);
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_secs(1);
            m.on_rate(t, 3 * MBPS, Some(3 * MBPS), Some(0.0), 3 * MBPS);
        }
        assert_eq!(m.active(), StreamMode::Weak);
        for _ in 0..60 {
            t += Duration::from_secs(1);
            assert!(!m.on_rate(t, 3 * MBPS, Some(40 * MBPS), Some(0.0), 3 * MBPS));
        }
    }

    #[test]
    fn a_chosen_weak_stays_and_a_choice_applies_at_once() {
        let t0 = Instant::now();
        let mut m = ModeControl::new(t0, StreamMode::Game);
        m.choose(t0, StreamMode::Weak);
        let (t, switched) = run(&mut m, t0, 60, 12, 40, 0.0);
        assert!(!switched);
        assert_eq!(m.active(), StreamMode::Weak);
        m.choose(t, StreamMode::Desktop);
        assert_eq!(m.active(), StreamMode::Desktop);
    }
}
