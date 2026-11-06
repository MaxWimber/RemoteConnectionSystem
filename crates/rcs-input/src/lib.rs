//! Input into Windows: the [`InputSink`] trait, what is held down, and the `SendInput` sink (D-022).
//!
//! The agent's input thread filters `seq`, turns relative sums into deltas and hands [`InputEvent`]s to a sink.
//! [`Held`] remembers every key, button and touch contact that is down, so `release_all` can let go of them after
//! a disconnect, a lost focus or a dropped message.

pub mod gamepad;
pub mod sendinput;
pub mod touch;

pub use rcs_protocol::input::{MouseButton, TouchPhase, TouchPoint};

/// One input action, already checked by the protocol parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent<'a> {
    /// Position over the captured monitor, 0..65535 on both axes.
    MouseAbsolute {
        x: u16,
        y: u16,
    },
    /// Movement since the previous relative event.
    MouseRelative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// 120 units per wheel step; `dy` above zero scrolls up, `dx` above zero scrolls right (Windows convention).
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// PS/2 Set 1 scan code, 0xE0 in the high byte for extended keys.
    Key {
        scancode: u16,
        down: bool,
    },
    Text(&'a str),
    Touch(&'a [TouchPoint]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// Windows took fewer events than given: UIPI (an elevated window in front), a locked or secure desktop.
    Blocked { sent: u32, wanted: u32, code: u32 },
    /// Not available on this machine or not implemented yet.
    Unsupported(&'static str),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked { sent, wanted, code } => {
                write!(
                    f,
                    "Windows took {sent} of {wanted} input events (error {code})"
                )
            }
            Self::Unsupported(what) => write!(f, "{what} is not supported"),
        }
    }
}

impl std::error::Error for InputError {}

pub trait InputSink {
    fn handle(&mut self, event: &InputEvent<'_>) -> Result<(), InputError>;
    /// Lets go of every key, button and contact that is down. Safe to call at any time.
    fn release_all(&mut self) -> Result<(), InputError>;
    /// Called often by the input thread, also without events: keeps resting touch contacts alive.
    fn tick(&mut self) -> Result<(), InputError> {
        Ok(())
    }
}

/// Keys, buttons and touch contacts that are down. No heap: 512 scan codes, 5 buttons, 256 contact ids.
#[derive(Debug, Clone, Default)]
pub struct Held {
    keys: [u64; 8],
    buttons: u8,
    contacts: [u64; 4],
}

/// Index of a scan code in the key set: the low byte, plus 256 for extended keys.
fn key_index(scancode: u16) -> Option<usize> {
    match scancode >> 8 {
        0 => Some(usize::from(scancode & 0xff)),
        0xe0 => Some(256 + usize::from(scancode & 0xff)),
        _ => None,
    }
}

fn set_bit(words: &mut [u64], i: usize, on: bool) -> bool {
    let (w, b) = (i / 64, 1u64 << (i % 64));
    let was = words[w] & b != 0;
    if on {
        words[w] |= b;
    } else {
        words[w] &= !b;
    }
    was
}

impl Held {
    /// Records a key change. Returns false for a release of a key that is not down: there is nothing to send.
    pub fn key(&mut self, scancode: u16, down: bool) -> bool {
        match key_index(scancode) {
            Some(i) => {
                let was = set_bit(&mut self.keys, i, down);
                down || was
            }
            None => false,
        }
    }

    /// Records a button change. Returns false for a release of a button that is not down.
    pub fn button(&mut self, button: MouseButton, down: bool) -> bool {
        let bit = 1u8 << button.wire();
        let was = self.buttons & bit != 0;
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        down || was
    }

    /// Records a touch contact: down and move keep it, up and cancel end it.
    pub fn contact(&mut self, id: u8, phase: TouchPhase) {
        let on = matches!(phase, TouchPhase::Down | TouchPhase::Move);
        set_bit(&mut self.contacts, usize::from(id), on);
    }

    pub fn contact_down(&self, id: u8) -> bool {
        self.contacts[usize::from(id) / 64] & (1 << (id % 64)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.buttons == 0
            && self.keys.iter().all(|w| *w == 0)
            && self.contacts.iter().all(|w| *w == 0)
    }

    /// Everything held, emptied: keys as scan codes, then buttons, then contact ids.
    pub fn take(&mut self) -> (Vec<u16>, Vec<MouseButton>, Vec<u8>) {
        let mut keys = Vec::new();
        for i in 0..512 {
            if self.keys[i / 64] & (1 << (i % 64)) != 0 {
                keys.push(if i < 256 {
                    i as u16
                } else {
                    0xe000 | (i - 256) as u16
                });
            }
        }
        let buttons = MouseButton::ALL
            .into_iter()
            .filter(|b| self.buttons & (1 << b.wire()) != 0)
            .collect();
        let contacts = (0..=255u8).filter(|id| self.contact_down(*id)).collect();
        *self = Self::default();
        (keys, buttons, contacts)
    }
}

/// What a sink saw, for tests and for `--input record`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    MouseAbsolute { x: u16, y: u16 },
    MouseRelative { dx: i32, dy: i32 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i16, dy: i16 },
    Key { scancode: u16, down: bool },
    Text(String),
    Touch(Vec<TouchPoint>),
    ReleaseAll,
}

/// Keeps events instead of sending them. With `keep` off it only counts, which is what `--input record` needs.
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub keep: bool,
    pub events: Vec<Recorded>,
    pub count: u64,
    pub held: Held,
}

impl RecordingSink {
    pub fn keeping() -> Self {
        Self {
            keep: true,
            ..Self::default()
        }
    }
}

impl InputSink for RecordingSink {
    fn handle(&mut self, event: &InputEvent<'_>) -> Result<(), InputError> {
        self.count += 1;
        match *event {
            InputEvent::Key { scancode, down } => {
                self.held.key(scancode, down);
            }
            InputEvent::Button { button, down } => {
                self.held.button(button, down);
            }
            InputEvent::Touch(points) => {
                for p in points {
                    self.held.contact(p.id, p.phase);
                }
            }
            _ => {}
        }
        if self.keep {
            self.events.push(match *event {
                InputEvent::MouseAbsolute { x, y } => Recorded::MouseAbsolute { x, y },
                InputEvent::MouseRelative { dx, dy } => Recorded::MouseRelative { dx, dy },
                InputEvent::Button { button, down } => Recorded::Button { button, down },
                InputEvent::Wheel { dx, dy } => Recorded::Wheel { dx, dy },
                InputEvent::Key { scancode, down } => Recorded::Key { scancode, down },
                InputEvent::Text(t) => Recorded::Text(t.into()),
                InputEvent::Touch(p) => Recorded::Touch(p.to_vec()),
            });
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        self.held = Held::default();
        if self.keep {
            self.events.push(Recorded::ReleaseAll);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_tracks_keys_buttons_and_contacts() {
        let mut h = Held::default();
        assert!(h.is_empty());
        assert!(h.key(0x1e, true));
        assert!(h.key(0xe01d, true));
        assert!(h.key(0xe01d, true), "a repeat is still sent");
        assert!(!h.key(0x2a, false), "nothing to release");
        assert!(!h.key(0xe11d, true), "not a Set 1 code with E0");
        assert!(h.button(MouseButton::Back, true));
        assert!(!h.button(MouseButton::Left, false));
        h.contact(3, TouchPhase::Down);
        h.contact(200, TouchPhase::Move);
        h.contact(4, TouchPhase::Down);
        h.contact(4, TouchPhase::Up);
        assert!(h.contact_down(3) && h.contact_down(200) && !h.contact_down(4));
        let (keys, buttons, contacts) = h.take();
        assert_eq!(keys, [0x1e, 0xe01d]);
        assert_eq!(buttons, [MouseButton::Back]);
        assert_eq!(contacts, [3, 200]);
        assert!(h.is_empty());
        assert!(!h.key(0x1e, false), "released by take");
    }

    #[test]
    fn recording_sink_keeps_events_and_lets_go() {
        let mut s = RecordingSink::keeping();
        s.handle(&InputEvent::Key {
            scancode: 0x1e,
            down: true,
        })
        .unwrap();
        s.handle(&InputEvent::Text("ok")).unwrap();
        assert!(!s.held.is_empty());
        s.release_all().unwrap();
        assert!(s.held.is_empty());
        assert_eq!(
            s.events,
            [
                Recorded::Key {
                    scancode: 0x1e,
                    down: true
                },
                Recorded::Text("ok".into()),
                Recorded::ReleaseAll
            ]
        );
        assert_eq!(s.count, 2);
    }
}
