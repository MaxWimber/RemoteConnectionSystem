//! The input thread (D-022): messages from the data channels -> `seq` filter and relative sums -> `InputSink`.
//!
//! It lets go of every key, button and contact when the media session ends or changes, when the network thread
//! asks for it (an input channel closed, a message was dropped), on `ReleaseAll` and when the agent stops. Messages
//! of a session that is no longer open are dropped, so nothing presses a key after the release.
//!
//! It also turns new pointer shapes from the capture thread into `CursorShape` messages (P4-7, D-022).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rcs_capture::PointerShape;
use rcs_input::sendinput::{Geometry, SendInputSink};
use rcs_input::{InputError, InputEvent, InputSink, RecordingSink};
use rcs_protocol::input::{ClientMessage, SeqFilter, ServerMessage};
use rcs_webrtc::{FrameSink, InputMessage};
use tracing::{debug, info, warn};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

/// How often the thread looks at the release flag and the live session when no message comes.
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Parse, check and count, but send nothing to Windows.
    Record,
    SendInput,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "record" => Some(Self::Record),
            "sendinput" => Some(Self::SendInput),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::SendInput => "sendinput",
        }
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    /// Events handed to the sink.
    pub events: AtomicU64,
    /// Messages of a session that had already ended, and old `seq`.
    pub stale: AtomicU64,
    /// Events the sink refused or Windows did not take.
    pub errors: AtomicU64,
    pub releases: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

/// `seq` filters and relative sums of one media session.
#[derive(Debug, Default)]
struct Session {
    number: u64,
    absolute: SeqFilter,
    relative: SeqFilter,
    /// The sums of the last relative move applied; the sums start at zero with the session.
    sums: (i32, i32),
}

impl Session {
    fn new(number: u64) -> Self {
        Self {
            number,
            ..Self::default()
        }
    }
}

/// What one message does: an event for the sink, a release, or nothing.
#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    Event(InputEvent<'a>),
    ReleaseAll,
    /// An old `seq`.
    Stale,
    /// Not for this phase: gamepads (phase 5), Ctrl+Alt+Del (phase 6), monitor switching.
    Ignored,
}

fn action<'a>(session: &mut Session, message: &'a ClientMessage) -> Action<'a> {
    match message {
        ClientMessage::MouseMoveAbsolute { seq, x, y } => {
            if session.absolute.accept(*seq) {
                Action::Event(InputEvent::MouseAbsolute { x: *x, y: *y })
            } else {
                Action::Stale
            }
        }
        ClientMessage::MouseMoveRelative { seq, dx, dy } => {
            if !session.relative.accept(*seq) {
                return Action::Stale;
            }
            // The difference from the last sums includes the moves of any packets lost in between.
            let delta = (
                dx.wrapping_sub(session.sums.0),
                dy.wrapping_sub(session.sums.1),
            );
            session.sums = (*dx, *dy);
            Action::Event(InputEvent::MouseRelative {
                dx: delta.0,
                dy: delta.1,
            })
        }
        ClientMessage::Key { down, scancode } => Action::Event(InputEvent::Key {
            scancode: *scancode,
            down: *down,
        }),
        ClientMessage::MouseButton { button, down } => Action::Event(InputEvent::Button {
            button: *button,
            down: *down,
        }),
        ClientMessage::Wheel { dx, dy } => Action::Event(InputEvent::Wheel { dx: *dx, dy: *dy }),
        ClientMessage::Touch { points } => Action::Event(InputEvent::Touch(points.as_slice())),
        ClientMessage::Text { text } => Action::Event(InputEvent::Text(text)),
        ClientMessage::ReleaseAll => Action::ReleaseAll,
        ClientMessage::GamepadState(_) | ClientMessage::Special { .. } => Action::Ignored,
    }
}

/// The primary monitor, for when the capture has not reported its monitor yet.
fn primary_monitor() -> [i32; 4] {
    // SAFETY: GetSystemMetrics takes no pointers.
    unsafe {
        [
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        ]
    }
}

pub struct Thread {
    pub mode: Mode,
    pub receiver: Receiver<InputMessage>,
    pub release: Arc<AtomicBool>,
    pub live: Arc<AtomicU64>,
    /// The captured monitor in desktop pixels, from the capture thread.
    pub monitor: Arc<dyn Fn() -> Option<[i32; 4]> + Send + Sync>,
    pub stats: Arc<Stats>,
    pub stop: Arc<AtomicBool>,
    /// New pointer shapes from the capture thread, and where their messages go.
    pub cursor: Option<(Receiver<PointerShape>, FrameSink)>,
}

impl Thread {
    pub fn spawn(self) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || self.run())
    }

    fn geometry(&self) -> Geometry {
        Geometry::with_current_desktop((self.monitor)().unwrap_or_else(primary_monitor))
    }

    fn run(self) {
        // SAFETY: the pseudo handle of the current thread needs no closing.
        if let Err(e) =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
        {
            warn!(error = %e, "input thread priority");
        }
        let mut sink: Box<dyn InputSink> = match self.mode {
            Mode::Record => Box::new(RecordingSink::default()),
            Mode::SendInput => Box::new(SendInputSink::new(self.geometry())),
        };
        info!(mode = self.mode.name(), "input thread started");
        let mut session = Session::default();
        let mut held_anything = false;
        let mut shape_id = 0u16;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            self.forward_cursor_shape(&mut shape_id);
            match self.receiver.recv_timeout(POLL) {
                Ok(m) => {
                    if m.session != self.live.load(Ordering::Relaxed) {
                        self.stats.stale.fetch_add(1, Ordering::Relaxed);
                    } else {
                        if m.session != session.number {
                            // A new browser session: nothing from the old one stays down, and its seq starts over.
                            self.release_all(sink.as_mut());
                            session = Session::new(m.session);
                            if self.mode == Mode::SendInput {
                                let g = self.geometry();
                                info!(monitor = ?g.monitor, desktop = ?g.desktop, "input geometry");
                                sink = Box::new(SendInputSink::new(g));
                            }
                        }
                        held_anything = true;
                        self.apply(sink.as_mut(), &mut session, &m.message);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let ended = session.number != 0 && self.live.load(Ordering::Relaxed) != session.number;
            if ended {
                session = Session::default();
            }
            if self.release.swap(false, Ordering::Relaxed) || ended {
                if held_anything {
                    self.release_all(sink.as_mut());
                }
                held_anything = false;
            }
        }
        self.release_all(sink.as_mut());
        info!("input thread stopped");
    }

    fn apply(&self, sink: &mut dyn InputSink, session: &mut Session, message: &ClientMessage) {
        match action(session, message) {
            Action::Event(event) => {
                self.stats.events.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = sink.handle(&event) {
                    self.note_error(&e);
                }
            }
            Action::ReleaseAll => self.release_all(sink),
            Action::Stale => {
                self.stats.stale.fetch_add(1, Ordering::Relaxed);
            }
            Action::Ignored => debug!(?message, "input message not handled in this phase"),
        }
    }

    /// Encodes the newest waiting pointer shape, if any. Older ones in the queue are skipped.
    fn forward_cursor_shape(&self, shape_id: &mut u16) {
        let Some((shapes, frames)) = &self.cursor else {
            return;
        };
        let Some(shape) = shapes.try_iter().last() else {
            return;
        };
        let Some(e) = crate::cursor::encode(&shape) else {
            debug!(
                kind = shape.kind,
                width = shape.width,
                height = shape.height,
                "pointer shape skipped"
            );
            return;
        };
        *shape_id = shape_id.wrapping_add(1);
        let mut message = Vec::with_capacity(11 + e.png.len());
        let fits = ServerMessage::CursorShape {
            shape_id: *shape_id,
            hot_x: e.hot_x,
            hot_y: e.hot_y,
            width: e.width,
            height: e.height,
            png: e.png,
        }
        .encode(&mut message);
        if fits {
            frames.set_cursor_shape(message);
        }
    }

    fn release_all(&self, sink: &mut dyn InputSink) {
        self.stats.releases.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = sink.release_all() {
            self.note_error(&e);
        }
    }

    fn note_error(&self, e: &InputError) {
        let n = self.stats.errors.fetch_add(1, Ordering::Relaxed);
        // The first error and then every thousandth: a locked desktop refuses every event.
        if n.is_multiple_of(1000) {
            warn!(error = %e, count = n + 1, "input not delivered");
        }
        if let Ok(mut last) = self.stats.last_error.lock() {
            *last = Some(e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;
    use std::time::Instant;

    use rcs_protocol::input::MouseButton;

    use super::*;

    #[test]
    fn relative_sums_become_deltas_and_old_seq_is_dropped() {
        let mut s = Session::new(1);
        let rel = |seq, dx, dy| ClientMessage::MouseMoveRelative { seq, dx, dy };
        assert_eq!(
            action(&mut s, &rel(1, 5, -2)),
            Action::Event(InputEvent::MouseRelative { dx: 5, dy: -2 })
        );
        // seq 2 is lost; 3 carries its movement too.
        assert_eq!(
            action(&mut s, &rel(3, 12, -2)),
            Action::Event(InputEvent::MouseRelative { dx: 7, dy: 0 })
        );
        assert_eq!(action(&mut s, &rel(2, 9, -2)), Action::Stale, "late");
        // Sums wrap around i32 without a jump.
        let mut s = Session::new(2);
        s.sums = (i32::MAX, 0);
        s.relative.accept(10);
        assert_eq!(
            action(&mut s, &rel(11, i32::MIN, 0)),
            Action::Event(InputEvent::MouseRelative { dx: 1, dy: 0 })
        );
        let abs = |seq| ClientMessage::MouseMoveAbsolute { seq, x: 1, y: 2 };
        assert!(matches!(action(&mut s, &abs(65535)), Action::Event(_)));
        assert!(matches!(action(&mut s, &abs(0)), Action::Event(_)), "wraps");
        assert_eq!(action(&mut s, &abs(65535)), Action::Stale);
        assert_eq!(
            action(&mut s, &ClientMessage::ReleaseAll),
            Action::ReleaseAll
        );
        assert_eq!(
            action(&mut s, &ClientMessage::Special { code: 1 }),
            Action::Ignored
        );
    }

    fn message(session: u64, message: ClientMessage) -> InputMessage {
        InputMessage { session, message }
    }

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_thread_lets_go_when_the_session_ends_and_drops_stale_messages() {
        let (sender, receiver) = sync_channel(16);
        let release = Arc::new(AtomicBool::new(false));
        let live = Arc::new(AtomicU64::new(1));
        let stats = Arc::new(Stats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = Thread {
            mode: Mode::Record,
            receiver,
            release: release.clone(),
            live: live.clone(),
            monitor: Arc::new(|| None),
            stats: stats.clone(),
            stop: stop.clone(),
            cursor: None,
        }
        .spawn()
        .unwrap();
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        let key = ClientMessage::Key {
            down: true,
            scancode: 0x1e,
        };
        sender.send(message(1, key.clone())).unwrap();
        wait_for("the first event", || load(&stats.events) == 1);
        let after_start = load(&stats.releases);

        // The session ends: the thread lets go without any message.
        live.store(0, Ordering::Relaxed);
        wait_for("the release", || load(&stats.releases) == after_start + 1);
        // A message of the ended session still in the queue does nothing.
        sender.send(message(1, key.clone())).unwrap();
        wait_for("the stale count", || load(&stats.stale) == 1);
        assert_eq!(load(&stats.events), 1);

        // A new session; the network thread's release flag works too.
        live.store(2, Ordering::Relaxed);
        sender
            .send(message(
                2,
                ClientMessage::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                },
            ))
            .unwrap();
        wait_for("the second event", || load(&stats.events) == 2);
        let before = load(&stats.releases);
        release.store(true, Ordering::Relaxed);
        wait_for("the flag release", || load(&stats.releases) > before);

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert_eq!(load(&stats.errors), 0);
    }
}
