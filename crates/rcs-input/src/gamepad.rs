//! Gamepads (phase 5, D-024): the [`GamepadBackend`] trait, a recording backend for tests, Xbox 360 controllers on
//! ViGEmBus, and [`PadManager`] with the rules of `docs/PROTOCOL.md`: `seq` per pad, zero after 500 ms of silence.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rcs_protocol::input::{GamepadState, SeqFilter};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

use crate::InputError;

/// Pads 0..3, as XInput has.
pub const MAX_PADS: usize = 4;
/// A pad that hears nothing for this long is set to neutral (PROTOCOL.md).
pub const SILENCE: Duration = Duration::from_millis(500);

/// Buttons, triggers and sticks of one pad, as XInput has them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

impl From<&GamepadState> for PadState {
    fn from(g: &GamepadState) -> Self {
        Self {
            buttons: g.buttons,
            left_trigger: g.left_trigger,
            right_trigger: g.right_trigger,
            left_x: g.left_x,
            left_y: g.left_y,
            right_x: g.right_x,
            right_y: g.right_y,
        }
    }
}

pub trait GamepadBackend {
    /// Sets a pad; the first state of a pad connects it.
    fn update(&mut self, pad: u8, state: &PadState) -> Result<(), InputError>;
    /// Unplugs a pad. Nothing happens for a pad that is not connected.
    fn disconnect(&mut self, pad: u8) -> Result<(), InputError>;
    /// `vigem` or `record`, for the status.
    fn name(&self) -> &'static str;
}

/// What a recording backend saw: a state, or a disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadEvent {
    Update(u8, PadState),
    Disconnect(u8),
}

/// Keeps what happens instead of creating controllers: tests, `--input record` and a server without ViGEmBus.
#[derive(Debug, Default)]
pub struct RecordingGamepads {
    pub keep: bool,
    pub events: Vec<PadEvent>,
    pub connected: [bool; MAX_PADS],
}

impl RecordingGamepads {
    pub fn keeping() -> Self {
        Self {
            keep: true,
            ..Self::default()
        }
    }
}

impl GamepadBackend for RecordingGamepads {
    fn update(&mut self, pad: u8, state: &PadState) -> Result<(), InputError> {
        if let Some(c) = self.connected.get_mut(usize::from(pad)) {
            *c = true;
        }
        if self.keep {
            self.events.push(PadEvent::Update(pad, *state));
        }
        Ok(())
    }

    fn disconnect(&mut self, pad: u8) -> Result<(), InputError> {
        if let Some(c) = self.connected.get_mut(usize::from(pad)) {
            *c = false;
        }
        if self.keep {
            self.events.push(PadEvent::Disconnect(pad));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "record"
    }
}

fn vigem_error(e: vigem_client::Error) -> InputError {
    let code = match e {
        vigem_client::Error::WinError(c) | vigem_client::Error::BusAccessFailed(c) => c,
        _ => 0,
    };
    InputError::Blocked {
        sent: 0,
        wanted: 1,
        code,
    }
}

/// Xbox 360 controllers on ViGEmBus.
pub struct VigemGamepads {
    client: Arc<Client>,
    pads: [Option<Xbox360Wired<Arc<Client>>>; MAX_PADS],
}

impl VigemGamepads {
    /// Opens the bus. The error says why in words for the status and the client.
    pub fn connect() -> Result<Self, String> {
        let client = Client::connect().map_err(|e| match e {
            vigem_client::Error::BusNotFound => "ViGEmBus не установлен".to_string(),
            vigem_client::Error::BusVersionMismatch => "версия ViGEmBus не подходит".to_string(),
            other => format!("ViGEmBus недоступен: {other}"),
        })?;
        Ok(Self {
            client: Arc::new(client),
            pads: Default::default(),
        })
    }
}

impl GamepadBackend for VigemGamepads {
    fn update(&mut self, pad: u8, state: &PadState) -> Result<(), InputError> {
        let Some(slot) = self.pads.get_mut(usize::from(pad)) else {
            return Ok(());
        };
        if slot.is_none() {
            let mut target = Xbox360Wired::new(self.client.clone(), TargetId::XBOX360_WIRED);
            target.plugin().map_err(vigem_error)?;
            target.wait_ready().map_err(vigem_error)?;
            *slot = Some(target);
        }
        let report = XGamepad {
            buttons: XButtons(state.buttons),
            left_trigger: state.left_trigger,
            right_trigger: state.right_trigger,
            thumb_lx: state.left_x,
            thumb_ly: state.left_y,
            thumb_rx: state.right_x,
            thumb_ry: state.right_y,
        };
        match slot.as_mut() {
            Some(target) => target.update(&report).map_err(vigem_error),
            None => Ok(()),
        }
    }

    fn disconnect(&mut self, pad: u8) -> Result<(), InputError> {
        match self.pads.get_mut(usize::from(pad)).and_then(Option::take) {
            Some(mut target) => target.unplug().map_err(vigem_error),
            None => Ok(()),
        }
    }

    fn name(&self) -> &'static str {
        "vigem"
    }
}

/// The pads of one media session.
#[derive(Debug, Default)]
pub struct PadManager {
    filters: [SeqFilter; MAX_PADS],
    /// When each pad last heard a state; None for a pad that never did in this session.
    heard: [Option<Instant>; MAX_PADS],
    zeroed: [bool; MAX_PADS],
}

/// What [`PadManager::state`] did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadOutcome {
    Applied,
    /// An old `seq` or a pad number over 3.
    Dropped,
}

impl PadManager {
    pub fn state(
        &mut self,
        g: &GamepadState,
        now: Instant,
        backend: &mut dyn GamepadBackend,
    ) -> Result<PadOutcome, InputError> {
        let i = usize::from(g.pad);
        if i >= MAX_PADS || !self.filters[i].accept(g.seq) {
            return Ok(PadOutcome::Dropped);
        }
        self.heard[i] = Some(now);
        self.zeroed[i] = false;
        backend.update(g.pad, &PadState::from(g))?;
        Ok(PadOutcome::Applied)
    }

    /// Sets pads that have been silent for `SILENCE` to neutral. Call it often.
    pub fn tick(
        &mut self,
        now: Instant,
        backend: &mut dyn GamepadBackend,
    ) -> Result<(), InputError> {
        for i in 0..MAX_PADS {
            if let Some(at) = self.heard[i]
                && !self.zeroed[i]
                && now.duration_since(at) >= SILENCE
            {
                self.zeroed[i] = true;
                backend.update(i as u8, &PadState::default())?;
            }
        }
        Ok(())
    }

    /// Sets every pad of this session to neutral: a disconnect, a lost focus, `ReleaseAll`.
    pub fn release(&mut self, backend: &mut dyn GamepadBackend) -> Result<(), InputError> {
        let mut result = Ok(());
        for i in 0..MAX_PADS {
            if self.heard[i].is_some() && !self.zeroed[i] {
                self.zeroed[i] = true;
                result = result.and(backend.update(i as u8, &PadState::default()));
            }
        }
        result
    }

    /// The session is over: neutral, then unplugged, and the `seq` counters start over.
    pub fn end(&mut self, backend: &mut dyn GamepadBackend) -> Result<(), InputError> {
        let mut result = self.release(backend);
        for i in 0..MAX_PADS {
            if self.heard[i].is_some() {
                result = result.and(backend.disconnect(i as u8));
            }
        }
        *self = Self::default();
        result
    }

    /// Pads that took a state in this session.
    pub fn active(&self) -> u32 {
        self.heard.iter().filter(|h| h.is_some()).count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(pad: u8, seq: u16, buttons: u16, left_x: i16) -> GamepadState {
        GamepadState {
            seq,
            pad,
            buttons,
            left_trigger: 10,
            right_trigger: 0,
            left_x,
            left_y: -5,
            right_x: 0,
            right_y: 32767,
        }
    }

    #[test]
    fn states_reach_the_backend_and_old_seq_is_dropped() {
        let mut b = RecordingGamepads::keeping();
        let mut m = PadManager::default();
        let t = Instant::now();
        assert_eq!(
            m.state(&g(0, 1, 0x1000, 100), t, &mut b),
            Ok(PadOutcome::Applied)
        );
        assert_eq!(
            m.state(&g(1, 1, 0x2000, -100), t, &mut b),
            Ok(PadOutcome::Applied),
            "own seq per pad"
        );
        assert_eq!(
            m.state(&g(0, 1, 0, 0), t, &mut b),
            Ok(PadOutcome::Dropped),
            "repeat"
        );
        assert_eq!(
            m.state(&g(0, 65535, 0, 0), t, &mut b),
            Ok(PadOutcome::Dropped),
            "older"
        );
        assert_eq!(
            m.state(&g(4, 1, 0, 0), t, &mut b),
            Ok(PadOutcome::Dropped),
            "no pad 4"
        );
        assert_eq!(
            b.events,
            [
                PadEvent::Update(
                    0,
                    PadState {
                        buttons: 0x1000,
                        left_trigger: 10,
                        right_trigger: 0,
                        left_x: 100,
                        left_y: -5,
                        right_x: 0,
                        right_y: 32767
                    }
                ),
                PadEvent::Update(
                    1,
                    PadState {
                        buttons: 0x2000,
                        left_trigger: 10,
                        right_trigger: 0,
                        left_x: -100,
                        left_y: -5,
                        right_x: 0,
                        right_y: 32767
                    }
                ),
            ]
        );
        assert_eq!(m.active(), 2);
    }

    #[test]
    fn silence_zeroes_once_and_a_new_state_wakes_the_pad() {
        let mut b = RecordingGamepads::keeping();
        let mut m = PadManager::default();
        let t = Instant::now();
        m.state(&g(2, 7, 0xffff, 32767), t, &mut b).unwrap();
        m.tick(t + SILENCE - Duration::from_millis(1), &mut b)
            .unwrap();
        assert_eq!(b.events.len(), 1, "not yet");
        m.tick(t + SILENCE, &mut b).unwrap();
        m.tick(t + SILENCE * 3, &mut b).unwrap();
        assert_eq!(b.events[1], PadEvent::Update(2, PadState::default()));
        assert_eq!(b.events.len(), 2, "zeroed once");
        assert!(b.connected[2], "still plugged in");
        m.state(&g(2, 8, 1, 0), t + SILENCE * 4, &mut b).unwrap();
        m.tick(t + SILENCE * 4 + Duration::from_millis(100), &mut b)
            .unwrap();
        assert_eq!(b.events.len(), 3);
    }

    #[test]
    fn release_zeroes_and_end_unplugs() {
        let mut b = RecordingGamepads::keeping();
        let mut m = PadManager::default();
        let t = Instant::now();
        m.state(&g(0, 1, 0x1000, 5), t, &mut b).unwrap();
        m.state(&g(3, 1, 0x8000, 5), t, &mut b).unwrap();
        m.release(&mut b).unwrap();
        assert_eq!(
            &b.events[2..],
            [
                PadEvent::Update(0, PadState::default()),
                PadEvent::Update(3, PadState::default())
            ]
        );
        b.events.clear();
        m.end(&mut b).unwrap();
        assert_eq!(
            b.events,
            [PadEvent::Disconnect(0), PadEvent::Disconnect(3)],
            "already neutral, then unplugged"
        );
        assert_eq!(b.connected, [false; MAX_PADS]);
        assert_eq!(m.active(), 0);
        assert_eq!(
            m.state(&g(0, 1, 0, 0), t, &mut b),
            Ok(PadOutcome::Applied),
            "seq starts over"
        );
    }
}
