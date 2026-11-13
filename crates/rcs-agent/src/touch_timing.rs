//! When touch messages arrive within a gesture (P11-1). A stall of the input channel shows as a long gap, and an up
//! that comes after one makes Windows see a finger held still: press-and-hold instead of a swipe. The gaps of a
//! finger that rests on the glass look the same, so a synthetic swipe with known timing tells them apart.

use std::time::{Duration, Instant};

use rcs_protocol::input::{TouchPhase, TouchPoint};

/// Gaps longer than these are counted.
const GAPS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(250),
];
/// An up after a gap this long counts as late.
const LATE_UP: Duration = Duration::from_millis(100);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TouchTiming {
    /// Ids that are down, one bit each.
    down: [u64; 4],
    last: Option<Instant>,
    pub gestures: u32,
    pub messages: u64,
    /// Gaps between two messages while a finger is down, over 50, 100 and 250 ms.
    pub gaps: [u32; 3],
    pub max_gap: Duration,
    pub late_ups: u32,
}

impl TouchTiming {
    fn any_down(&self) -> bool {
        self.down.iter().any(|w| *w != 0)
    }

    fn set(&mut self, id: u8, on: bool) {
        let (word, bit) = (usize::from(id / 64), id % 64);
        if on {
            self.down[word] |= 1 << bit;
        } else {
            self.down[word] &= !(1 << bit);
        }
    }

    /// One touch message that the agent took, from either channel.
    pub fn on_touch(&mut self, now: Instant, points: &[TouchPoint]) {
        self.messages += 1;
        if self.any_down()
            && let Some(last) = self.last
        {
            let gap = now.saturating_duration_since(last);
            for (count, limit) in self.gaps.iter_mut().zip(GAPS) {
                if gap > limit {
                    *count += 1;
                }
            }
            self.max_gap = self.max_gap.max(gap);
            let up = points
                .iter()
                .any(|p| matches!(p.phase, TouchPhase::Up | TouchPhase::Cancel));
            if up && gap > LATE_UP {
                self.late_ups += 1;
            }
        } else if points.iter().any(|p| p.phase == TouchPhase::Down) {
            self.gestures += 1;
        }
        for p in points {
            self.set(p.id, matches!(p.phase, TouchPhase::Down | TouchPhase::Move));
        }
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: u8, phase: TouchPhase) -> TouchPoint {
        TouchPoint {
            id,
            phase,
            x: 0,
            y: 0,
        }
    }

    #[test]
    fn gaps_inside_a_gesture_count_and_pauses_between_do_not() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = TouchTiming::default();
        t.on_touch(ms(0), &[p(1, TouchPhase::Down)]);
        t.on_touch(ms(16), &[p(1, TouchPhase::Move)]);
        t.on_touch(ms(140), &[p(1, TouchPhase::Move)]);
        t.on_touch(ms(420), &[p(1, TouchPhase::Up)]);
        // A pause with no finger down, then a second gesture.
        t.on_touch(ms(5000), &[p(2, TouchPhase::Down)]);
        t.on_touch(ms(5016), &[p(2, TouchPhase::Up)]);
        assert_eq!(t.gestures, 2);
        assert_eq!(t.messages, 6);
        assert_eq!(t.gaps, [2, 2, 1]);
        assert_eq!(t.max_gap, Duration::from_millis(280));
        assert_eq!(t.late_ups, 1);
        // An up of an id that is not down, as a repeat on the other channel, starts nothing.
        t.on_touch(ms(9000), &[p(2, TouchPhase::Up)]);
        assert_eq!(t.gestures, 2);
        assert_eq!(t.max_gap, Duration::from_millis(280));
    }
}
