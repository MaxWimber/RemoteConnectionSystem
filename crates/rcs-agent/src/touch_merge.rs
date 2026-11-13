//! Touches from two channels into one stream for the sink (P11-1, D-048).
//!
//! The client sends a finger's moves on `input-fast` only (`TouchFast`, with a `seq`), so a lost datagram does not
//! hold the next ones back as on the ordered `input-reliable`. Downs and ups go on both channels: `input-fast` is
//! quick, `input-reliable` makes sure they arrive. This filter passes each down and up once, whichever copy comes
//! first, and drops what comes too late:
//! - a reliable down whose fast copy already came, or whose finger already ended by a fast up, is a copy;
//! - a reliable up whose fast copy already lifted the finger is a copy;
//! - moves and ups of a finger that is not down are dropped, as the sink does.
//!
//! The client gives every new finger the next id of 0..=255, so the marks of an old finger are gone long before its
//! id comes again. An old client that sends only `Touch` sets no marks and passes as before.

use rcs_protocol::input::{TouchPhase, TouchPoint};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bits([u64; 4]);

impl Bits {
    fn get(&self, id: u8) -> bool {
        self.0[usize::from(id / 64)] & (1 << (id % 64)) != 0
    }

    fn set(&mut self, id: u8, on: bool) {
        let word = &mut self.0[usize::from(id / 64)];
        if on {
            *word |= 1 << (id % 64);
        } else {
            *word &= !(1 << (id % 64));
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TouchMerge {
    down: Bits,
    /// A fast down came; its reliable copy has not.
    fast_down: Bits,
    /// A fast up lifted the finger; its reliable copy has not come.
    fast_up: Bits,
    /// A fast up came for a finger that was not down yet: its reliable down, when it comes, is too late.
    gone: Bits,
}

impl TouchMerge {
    /// Everything is let go: the sink has no contacts any more.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The points of one message that the sink should get, in `out`.
    pub fn filter(&mut self, fast: bool, points: &[TouchPoint], out: &mut Vec<TouchPoint>) {
        out.clear();
        for p in points {
            if self.pass(fast, p) {
                out.push(*p);
            }
        }
    }

    fn pass(&mut self, fast: bool, p: &TouchPoint) -> bool {
        let id = p.id;
        match (p.phase, fast) {
            (TouchPhase::Down, true) => {
                if self.down.get(id) || self.gone.get(id) {
                    return false;
                }
                self.down.set(id, true);
                self.fast_down.set(id, true);
                true
            }
            (TouchPhase::Down, false) => {
                if self.fast_down.get(id) {
                    self.fast_down.set(id, false);
                    return false;
                }
                if self.gone.get(id) || self.down.get(id) {
                    return false;
                }
                self.down.set(id, true);
                true
            }
            (TouchPhase::Move, _) => self.down.get(id),
            (TouchPhase::Up | TouchPhase::Cancel, true) => {
                if self.down.get(id) {
                    self.down.set(id, false);
                    self.fast_up.set(id, true);
                    true
                } else {
                    // A repeat of a fast up that already lifted it, or an up ahead of its down.
                    if !self.fast_up.get(id) {
                        self.gone.set(id, true);
                    }
                    false
                }
            }
            (TouchPhase::Up | TouchPhase::Cancel, false) => {
                self.fast_down.set(id, false);
                if self.fast_up.get(id) {
                    self.fast_up.set(id, false);
                    return false;
                }
                if self.gone.get(id) {
                    self.gone.set(id, false);
                    return false;
                }
                let was = self.down.get(id);
                self.down.set(id, false);
                was
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: u8, phase: TouchPhase) -> TouchPoint {
        TouchPoint {
            id,
            phase,
            x: 1,
            y: 2,
        }
    }

    /// The phases the sink gets for a sequence of (fast, point).
    fn run(m: &mut TouchMerge, steps: &[(bool, TouchPoint)]) -> Vec<TouchPhase> {
        let mut out = Vec::new();
        let mut got = Vec::new();
        for (fast, point) in steps {
            m.filter(*fast, std::slice::from_ref(point), &mut out);
            got.extend(out.iter().map(|q| q.phase));
        }
        got
    }

    use TouchPhase::{Down, Move, Up};

    #[test]
    fn each_down_and_up_passes_once_whichever_copy_comes_first() {
        let mut m = TouchMerge::default();
        let got = run(
            &mut m,
            &[
                (true, p(1, Down)),
                (false, p(1, Down)),
                (true, p(1, Move)),
                (true, p(1, Up)),
                (true, p(1, Up)),
                (false, p(1, Up)),
            ],
        );
        assert_eq!(got, [Down, Move, Up]);
        // The fast copies lost: the reliable ones do the work.
        let got = run(
            &mut m,
            &[(false, p(2, Down)), (true, p(2, Move)), (false, p(2, Up))],
        );
        assert_eq!(got, [Down, Move, Up]);
    }

    #[test]
    fn a_late_reliable_down_after_the_gesture_ended_is_not_a_tap() {
        // The fast down lost, the reliable one held back behind a lost datagram: the fast up comes first.
        let mut m = TouchMerge::default();
        let got = run(
            &mut m,
            &[
                (true, p(3, Move)),
                (true, p(3, Up)),
                (false, p(3, Down)),
                (false, p(3, Up)),
            ],
        );
        assert!(got.is_empty(), "{got:?}");
        // The marks are gone: the id can start a finger again.
        assert_eq!(
            run(&mut m, &[(false, p(3, Down)), (false, p(3, Up))]),
            [Down, Up]
        );
    }

    #[test]
    fn an_old_client_with_only_reliable_touches_passes_as_before() {
        let mut m = TouchMerge::default();
        let got = run(
            &mut m,
            &[
                (false, p(0, Down)),
                (false, p(0, Move)),
                (false, p(0, Up)),
                (false, p(0, Down)),
                (false, p(0, Up)),
                (false, p(5, Move)),
            ],
        );
        assert_eq!(got, [Down, Move, Up, Down, Up]);
    }

    #[test]
    fn clear_forgets_fingers() {
        let mut m = TouchMerge::default();
        run(&mut m, &[(true, p(9, Down))]);
        m.clear();
        assert!(run(&mut m, &[(true, p(9, Move))]).is_empty());
        assert_eq!(run(&mut m, &[(false, p(9, Down))]), [Down]);
    }
}
