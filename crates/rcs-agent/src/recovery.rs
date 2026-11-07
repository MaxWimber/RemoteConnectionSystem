//! When the pipeline is built again after an error, and when `auto` gives up on AMF for a while (D-028).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The first pause after an error.
const FIRST_PAUSE: Duration = Duration::from_millis(250);
const LONGEST_PAUSE: Duration = Duration::from_secs(5);
/// A pipeline that worked this long before the error starts again from the first pause.
const HEALTHY_FOR: Duration = Duration::from_secs(30);

/// Encoder failures of AMF that switch `auto` to Media Foundation, and the window they must fall in.
const AMF_FAILURES: usize = 3;
const AMF_WINDOW: Duration = Duration::from_secs(60);

/// Pause before the next attempt: doubles after each quick failure, resets after a pipeline that worked.
#[derive(Debug, Default)]
pub struct Backoff {
    next: Option<Duration>,
}

impl Backoff {
    /// The pause after an attempt that ran for `ran` and then failed.
    pub fn after_failure(&mut self, ran: Duration) -> Duration {
        let pause = match self.next {
            Some(next) if ran < HEALTHY_FOR => next,
            _ => FIRST_PAUSE,
        };
        self.next = Some((pause * 2).min(LONGEST_PAUSE));
        pause
    }

    pub fn reset(&mut self) {
        self.next = None;
    }
}

/// Counts AMF encoder failures in a sliding window.
#[derive(Debug, Default)]
pub struct AmfFailures {
    at: VecDeque<Instant>,
}

impl AmfFailures {
    /// Records one failure and says whether `auto` should leave AMF now.
    pub fn record(&mut self, now: Instant) -> bool {
        while self
            .at
            .front()
            .is_some_and(|t| now.duration_since(*t) >= AMF_WINDOW)
        {
            self.at.pop_front();
        }
        self.at.push_back(now);
        self.at.len() >= AMF_FAILURES
    }

    pub fn clear(&mut self) {
        self.at.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_doubles_up_to_five_seconds() {
        let mut b = Backoff::default();
        let quick = Duration::from_millis(100);
        let pauses: Vec<u128> = (0..7).map(|_| b.after_failure(quick).as_millis()).collect();
        assert_eq!(pauses, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    }

    #[test]
    fn a_pipeline_that_worked_starts_from_the_first_pause() {
        let mut b = Backoff::default();
        b.after_failure(Duration::ZERO);
        b.after_failure(Duration::ZERO);
        assert_eq!(b.after_failure(HEALTHY_FOR), FIRST_PAUSE);
        assert_eq!(b.after_failure(Duration::ZERO), FIRST_PAUSE * 2);
        b.reset();
        assert_eq!(b.after_failure(Duration::ZERO), FIRST_PAUSE);
    }

    #[test]
    fn three_amf_failures_in_a_minute_leave_amf() {
        let t = Instant::now();
        let mut f = AmfFailures::default();
        assert!(!f.record(t));
        assert!(!f.record(t + Duration::from_secs(10)));
        assert!(f.record(t + Duration::from_secs(59)));
    }

    #[test]
    fn failures_further_apart_do_not_add_up() {
        let t = Instant::now();
        let mut f = AmfFailures::default();
        assert!(!f.record(t));
        assert!(!f.record(t + Duration::from_secs(40)));
        // The first one is a minute old by now.
        assert!(!f.record(t + Duration::from_secs(60)));
        assert!(f.record(t + Duration::from_secs(70)));
        f.clear();
        assert!(!f.record(t + Duration::from_secs(71)));
    }
}
