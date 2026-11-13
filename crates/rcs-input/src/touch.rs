//! Direct touch through `InjectTouchInput` (P4-9).
//!
//! Windows wants every contact that is down in each injected frame, and cancels a contact that is not updated for a
//! while. The client numbers fingers 0..=255 (P11-1); Windows gets the lowest free pointer id of 0..10 instead. [`Contacts`] keeps the contacts that are down and builds those frames; it is pure, so tests check it.
//! [`TouchInjector`] sends them and repeats the last frame while fingers rest.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GetLastError, POINT, RECT};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_FLAGS, POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PT_TOUCH, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
};

use crate::sendinput::Geometry;
use crate::{InputError, TouchPhase, TouchPoint};

/// Windows takes at most this many contacts; the protocol sends at most 10.
pub const MAX_CONTACTS: usize = 10;
/// A resting finger is injected again this often, so Windows does not cancel it.
pub const KEEP_ALIVE: Duration = Duration::from_millis(100);
/// Half the side of the contact rectangle, in pixels.
const CONTACT_RADIUS: i32 = 4;

/// One contact in an injected frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contact {
    /// The Windows pointer id, 0..MAX_CONTACTS.
    pub id: u8,
    pub x: i32,
    pub y: i32,
    /// `POINTER_FLAGS` bits.
    pub flags: u32,
}

const HELD: u32 = POINTER_FLAG_INRANGE.0 | POINTER_FLAG_INCONTACT.0;

/// The contacts that are down, in desktop pixels: the client's id, the Windows pointer id, the place.
#[derive(Debug, Default, Clone)]
pub struct Contacts {
    down: Vec<(u8, u8, i32, i32)>,
}

impl Contacts {
    pub fn is_empty(&self) -> bool {
        self.down.is_empty()
    }

    /// Applies a `Touch` message and returns the frame to inject: the changed contacts with their phase, and every
    /// other contact that is down as an update. A move or up of an unknown id and a second down are ignored.
    pub fn apply(&mut self, points: &[TouchPoint], geometry: &Geometry, out: &mut Vec<Contact>) {
        out.clear();
        for p in points {
            let (x, y) = geometry.pixel(p.x, p.y);
            let known = self.down.iter().position(|c| c.0 == p.id);
            let (slot, flags) = match (p.phase, known) {
                (TouchPhase::Down, None) if self.down.len() < MAX_CONTACTS => {
                    let slot = (0..MAX_CONTACTS as u8)
                        .find(|s| !self.down.iter().any(|c| c.1 == *s))
                        .unwrap_or(0);
                    self.down.push((p.id, slot, x, y));
                    (slot, POINTER_FLAG_DOWN.0 | HELD)
                }
                (TouchPhase::Move, Some(i)) => {
                    let slot = self.down[i].1;
                    self.down[i] = (p.id, slot, x, y);
                    (slot, POINTER_FLAG_UPDATE.0 | HELD)
                }
                (TouchPhase::Up, Some(i)) => (self.down.remove(i).1, POINTER_FLAG_UP.0),
                (TouchPhase::Cancel, Some(i)) => (
                    self.down.remove(i).1,
                    POINTER_FLAG_UP.0 | POINTER_FLAG_CANCELED.0,
                ),
                _ => continue,
            };
            let contact = Contact {
                id: slot,
                x,
                y,
                flags,
            };
            if let Some(c) = out.iter_mut().find(|c| c.id == slot) {
                // A down and an up of the same finger in one message: inject the latest.
                *c = contact;
            } else {
                out.push(contact);
            }
        }
        if out.is_empty() {
            return;
        }
        for &(_, id, x, y) in &self.down {
            if !out.iter().any(|c| c.id == id) {
                out.push(Contact {
                    id,
                    x,
                    y,
                    flags: POINTER_FLAG_UPDATE.0 | HELD,
                });
            }
        }
    }

    /// The frame that keeps resting contacts alive.
    pub fn keep_alive(&self, out: &mut Vec<Contact>) {
        out.clear();
        out.extend(self.down.iter().map(|&(_, id, x, y)| Contact {
            id,
            x,
            y,
            flags: POINTER_FLAG_UPDATE.0 | HELD,
        }));
    }

    /// The frame that lifts every contact, emptied.
    pub fn release(&mut self, out: &mut Vec<Contact>) {
        out.clear();
        out.extend(self.down.drain(..).map(|(_, id, x, y)| Contact {
            id,
            x,
            y,
            flags: POINTER_FLAG_UP.0 | POINTER_FLAG_CANCELED.0,
        }));
    }
}

fn to_info(c: &Contact) -> POINTER_TOUCH_INFO {
    POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: PT_TOUCH,
            pointerId: u32::from(c.id),
            ptPixelLocation: POINT { x: c.x, y: c.y },
            pointerFlags: POINTER_FLAGS(c.flags),
            ..Default::default()
        },
        touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
        rcContact: RECT {
            left: c.x - CONTACT_RADIUS,
            top: c.y - CONTACT_RADIUS,
            right: c.x + CONTACT_RADIUS,
            bottom: c.y + CONTACT_RADIUS,
        },
        orientation: 90,
        pressure: 32_000,
        ..Default::default()
    }
}

/// Injects touch frames. Created on the first touch: `InitializeTouchInjection` once per process.
pub struct TouchInjector {
    contacts: Contacts,
    frame: Vec<Contact>,
    infos: Vec<POINTER_TOUCH_INFO>,
    last: Instant,
}

impl TouchInjector {
    pub fn new() -> Result<Self, InputError> {
        // SAFETY: no pointers; allows up to MAX_CONTACTS contacts with the default visual feedback.
        unsafe { InitializeTouchInjection(MAX_CONTACTS as u32, TOUCH_FEEDBACK_DEFAULT) }.map_err(
            |e| InputError::Blocked {
                sent: 0,
                wanted: 1,
                code: e.code().0 as u32,
            },
        )?;
        Ok(Self {
            contacts: Contacts::default(),
            frame: Vec::with_capacity(MAX_CONTACTS),
            infos: Vec::with_capacity(MAX_CONTACTS),
            last: Instant::now(),
        })
    }

    pub fn touch(&mut self, points: &[TouchPoint], geometry: &Geometry) -> Result<(), InputError> {
        self.contacts.apply(points, geometry, &mut self.frame);
        self.inject()
    }

    /// Repeats the frame of resting contacts; call it often, it does nothing until `KEEP_ALIVE` passed.
    pub fn tick(&mut self) -> Result<(), InputError> {
        if self.contacts.is_empty() || self.last.elapsed() < KEEP_ALIVE {
            return Ok(());
        }
        self.contacts.keep_alive(&mut self.frame);
        self.inject()
    }

    pub fn release(&mut self) -> Result<(), InputError> {
        self.contacts.release(&mut self.frame);
        self.inject()
    }

    fn inject(&mut self) -> Result<(), InputError> {
        if self.frame.is_empty() {
            return Ok(());
        }
        self.last = Instant::now();
        self.infos.clear();
        self.infos.extend(self.frame.iter().map(to_info));
        // SAFETY: the slice holds initialized POINTER_TOUCH_INFOs, as many as the call is told.
        unsafe { InjectTouchInput(&self.infos) }.map_err(|e| {
            // Windows refuses frames for contacts it has ended itself, for example after an edge swipe of the
            // system. Forget them, so the next touch starts clean instead of failing again.
            self.contacts = Contacts::default();
            // SAFETY: no pointers; reads this thread's last error right after the call.
            let code = unsafe { GetLastError() }.0;
            InputError::Blocked {
                sent: 0,
                wanted: self.infos.len() as u32,
                code: if code == 0 { e.code().0 as u32 } else { code },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Geometry = Geometry {
        monitor: [0, 0, 1920, 1080],
        desktop: [0, 0, 1920, 1080],
    };

    fn p(id: u8, phase: TouchPhase, x: u16, y: u16) -> TouchPoint {
        TouchPoint { id, phase, x, y }
    }

    #[test]
    fn frames_carry_every_contact_that_is_down() {
        let mut c = Contacts::default();
        let mut f = Vec::new();
        c.apply(&[p(1, TouchPhase::Down, 0, 0)], &G, &mut f);
        assert_eq!(
            f,
            [Contact {
                id: 0,
                x: 0,
                y: 0,
                flags: POINTER_FLAG_DOWN.0 | HELD
            }]
        );
        // A second finger: the first one is reported too, as an update where it was.
        c.apply(&[p(200, TouchPhase::Down, 65535, 65535)], &G, &mut f);
        assert_eq!(f.len(), 2);
        assert_eq!(
            (f[0].id, f[0].x, f[0].flags),
            (1, 1919, POINTER_FLAG_DOWN.0 | HELD)
        );
        assert_eq!(
            (f[1].id, f[1].x, f[1].flags),
            (0, 0, POINTER_FLAG_UPDATE.0 | HELD)
        );
        c.apply(&[p(1, TouchPhase::Move, 32768, 0)], &G, &mut f);
        assert_eq!((f[0].id, f[0].x), (0, 960));
        c.apply(&[p(1, TouchPhase::Up, 32768, 0)], &G, &mut f);
        assert_eq!((f[0].id, f[0].flags), (0, POINTER_FLAG_UP.0));
        assert_eq!((f[1].id, f[1].flags), (1, POINTER_FLAG_UPDATE.0 | HELD));
        // The freed Windows id 0 goes to the next finger.
        c.apply(&[p(7, TouchPhase::Down, 0, 0)], &G, &mut f);
        assert_eq!(f[0].id, 0);
        c.apply(&[p(7, TouchPhase::Up, 0, 0)], &G, &mut f);
        c.apply(&[p(200, TouchPhase::Cancel, 0, 0)], &G, &mut f);
        assert_eq!(f[0].flags, POINTER_FLAG_UP.0 | POINTER_FLAG_CANCELED.0);
        assert!(c.is_empty());
    }

    #[test]
    fn nonsense_is_ignored() {
        let mut c = Contacts::default();
        let mut f = Vec::new();
        c.apply(
            &[p(5, TouchPhase::Move, 1, 1), p(6, TouchPhase::Up, 1, 1)],
            &G,
            &mut f,
        );
        assert!(f.is_empty(), "moves and ups of unknown fingers");
        c.apply(&[p(5, TouchPhase::Down, 1, 1)], &G, &mut f);
        c.apply(&[p(5, TouchPhase::Down, 9, 9)], &G, &mut f);
        assert!(f.is_empty(), "a second down of a finger that is down");
        let downs: Vec<TouchPoint> = (10..30).map(|id| p(id, TouchPhase::Down, 1, 1)).collect();
        c.apply(&downs, &G, &mut f);
        assert_eq!(f.len(), MAX_CONTACTS, "no more than Windows takes");
    }

    #[test]
    fn a_tap_in_one_message_is_its_up() {
        let mut c = Contacts::default();
        let mut f = Vec::new();
        c.apply(
            &[p(1, TouchPhase::Down, 0, 0), p(1, TouchPhase::Up, 0, 0)],
            &G,
            &mut f,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].flags, POINTER_FLAG_UP.0);
    }

    #[test]
    fn resting_fingers_are_kept_alive_and_released() {
        let mut c = Contacts::default();
        let mut f = Vec::new();
        c.apply(
            &[p(1, TouchPhase::Down, 0, 0), p(2, TouchPhase::Down, 9, 9)],
            &G,
            &mut f,
        );
        c.keep_alive(&mut f);
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|c| c.flags == POINTER_FLAG_UPDATE.0 | HELD));
        c.release(&mut f);
        assert_eq!(f.len(), 2);
        assert!(
            f.iter()
                .all(|c| c.flags == POINTER_FLAG_UP.0 | POINTER_FLAG_CANCELED.0)
        );
        assert!(c.is_empty());
        c.keep_alive(&mut f);
        assert!(f.is_empty());
    }

    #[test]
    fn contacts_become_pointer_infos() {
        let info = to_info(&Contact {
            id: 3,
            x: 100,
            y: 200,
            flags: POINTER_FLAG_DOWN.0 | HELD,
        });
        assert_eq!(info.pointerInfo.pointerType, PT_TOUCH);
        assert_eq!(info.pointerInfo.pointerId, 3);
        assert_eq!(
            (
                info.pointerInfo.ptPixelLocation.x,
                info.pointerInfo.ptPixelLocation.y
            ),
            (100, 200)
        );
        assert_eq!(
            info.rcContact.right - info.rcContact.left,
            2 * CONTACT_RADIUS
        );
    }
}
