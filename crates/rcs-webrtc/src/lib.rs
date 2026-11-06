//! WebRTC transport on `str0m` (D-020): ICE-lite on one UDP port, one session at a time, H.264 to the browser.
//!
//! All network work runs on one thread that owns the socket and the `Rtc`. The encoder hands frames over through
//! [`FrameSink`]: a queue of two frames. When it overflows, the oldest frame is dropped and an IDR is requested, so the
//! capture thread never waits for the network. A one-byte datagram from a loopback socket wakes the network thread
//! when a frame or a command arrives.
//!
//! Messages on `input-fast` and `input-reliable` are parsed here and handed to the agent's input thread through
//! [`InputLink`] without waiting (D-022). When that queue is full, or a session or an input channel ends, the link's
//! `release` flag asks the input thread to let go of everything.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcs_protocol::input::{ClientMessage, ParseError, ServerMessage};
use serde_json::{Value, json};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{AbsCaptureTime, Extension};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tracing::{debug, info, warn};

/// Frames waiting for the network thread. Two is enough to ride out one slow send without adding latency.
const QUEUE_FRAMES: usize = 2;
/// Spare frame buffers kept for the encoder.
const POOL_BUFFERS: usize = 4;
/// Longest wait of the network thread when nothing is scheduled.
const IDLE_WAIT: Duration = Duration::from_millis(50);
const WAKE: [u8; 1] = [0x5a];
/// Free in the standard map of str0m; the answer follows the ids of the offer anyway.
const PLAYOUT_DELAY_ID: u8 = 6;
const ABS_CAPTURE_TIME_ID: u8 = 7;
/// Largest datagram we expect; ICE, DTLS and RTCP from browsers fit easily.
const RECV_BUFFER: usize = 2048;

#[derive(Debug, Clone)]
pub struct Config {
    /// Local UDP address, for example `0.0.0.0:8443`.
    pub bind: SocketAddr,
    /// Host candidates written into the answer: the addresses browsers should send to.
    pub advertise: Vec<SocketAddr>,
    /// Testing only (P2-4): every `every` the server sends nothing for `burst`, like a short network outage.
    pub simulate_loss: Option<LossTest>,
    /// Where input messages go. Without it they are parsed, counted and dropped.
    pub input: Option<InputLink>,
}

/// One input message and the media session it came from: a new session number means new `seq` counters.
#[derive(Debug)]
pub struct InputMessage {
    pub session: u64,
    pub message: ClientMessage,
}

/// The network thread's end of the input queue.
#[derive(Debug, Clone)]
pub struct InputLink {
    pub sender: SyncSender<InputMessage>,
    /// Set when an input channel ends or a message was dropped: release every key and button.
    pub release: Arc<AtomicBool>,
    /// The number of the session that is open now, 0 when none is. Messages of any other session are stale: the
    /// input thread drops them and lets go of what the ended session held.
    pub live: Arc<AtomicU64>,
}

impl InputLink {
    fn release(&self) {
        self.release.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LossTest {
    pub every: Duration,
    pub burst: Duration,
}

impl LossTest {
    /// Whether a datagram sent `since` the session started falls into a simulated outage.
    pub fn drops(&self, since: Duration) -> bool {
        let every = self.every.as_nanos().max(1);
        since.as_nanos() % every < self.burst.as_nanos()
    }
}

/// One encoded access unit in Annex B.
#[derive(Debug)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    /// When the frame was captured; the RTP timestamp is derived from it.
    pub captured: Instant,
    pub keyframe: bool,
}

/// Counters for logs and the statistics panel.
#[derive(Debug, Default)]
pub struct Counters {
    pub sessions: AtomicU64,
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Dropped because the queue was full: the network thread fell behind.
    pub frames_dropped_queue: AtomicU64,
    /// Dropped because no browser was connected yet.
    pub frames_dropped_idle: AtomicU64,
    /// PLI and FIR from the browser.
    pub keyframe_requests: AtomicU64,
    pub write_errors: AtomicU64,
    /// Datagrams held back by `Config::simulate_loss`.
    pub datagrams_dropped_test: AtomicU64,
    /// Input messages handed to the input thread.
    pub input_messages: AtomicU64,
    /// Input messages of an unknown type, or malformed: wrong length, a field out of range, text on a binary channel.
    pub input_rejected: AtomicU64,
    /// Input messages dropped because the input queue was full.
    pub input_dropped: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CountersSnapshot {
    pub sessions: u64,
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub frames_dropped_queue: u64,
    pub frames_dropped_idle: u64,
    pub keyframe_requests: u64,
    pub write_errors: u64,
    pub datagrams_dropped_test: u64,
    pub input_messages: u64,
    pub input_rejected: u64,
    pub input_dropped: u64,
}

/// A cursor position packed in a u64: x in bits 0-15, y in 16-31, visible in bit 32, bit 40 once set at all.
fn pack_cursor(visible: bool, x: u16, y: u16) -> u64 {
    1 << 40 | u64::from(visible) << 32 | u64::from(y) << 16 | u64::from(x)
}

fn unpack_cursor(v: u64) -> Option<(bool, u16, u16)> {
    (v & 1 << 40 != 0).then_some((v & 1 << 32 != 0, v as u16, (v >> 16) as u16))
}

struct Shared {
    /// The latest pointer position, `pack_cursor`; 0 before the first.
    cursor_position: AtomicU64,
    /// A new `CursorShape` message waiting for the network thread.
    cursor_shape: Mutex<Option<Vec<u8>>>,
    queue: Mutex<VecDeque<VideoFrame>>,
    pool: Mutex<Vec<Vec<u8>>>,
    keyframe_wanted: AtomicBool,
    connected: AtomicBool,
    counters: Counters,
    waker: UdpSocket,
    wake_target: SocketAddr,
}

impl Shared {
    fn wake(&self) {
        let _ = self.waker.send_to(&WAKE, self.wake_target);
    }
}

/// The encoder's side of the transport. Cheap to clone.
#[derive(Clone)]
pub struct FrameSink {
    shared: Arc<Shared>,
}

impl FrameSink {
    /// An empty buffer for the next frame, reused from frames already sent.
    pub fn buffer(&self) -> Vec<u8> {
        self.shared
            .pool
            .lock()
            .ok()
            .and_then(|mut p| p.pop())
            .unwrap_or_default()
    }

    /// Queues a frame. Returns true when an older frame had to be dropped; an IDR is then requested.
    pub fn push(&self, frame: VideoFrame) -> bool {
        let mut dropped = false;
        if let Ok(mut queue) = self.shared.queue.lock()
            && let Some(old) = push_bounded(&mut queue, frame)
        {
            dropped = true;
            recycle(&self.shared, old.data);
        }
        if dropped {
            self.shared
                .counters
                .frames_dropped_queue
                .fetch_add(1, Ordering::Relaxed);
            self.shared.keyframe_wanted.store(true, Ordering::Relaxed);
        }
        self.shared.wake();
        dropped
    }

    /// True once per request: the browser asked for a keyframe, a session started, or frames were dropped.
    pub fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe_wanted.swap(false, Ordering::Relaxed)
    }

    /// Whether a browser is connected and frames are worth encoding.
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// The pointer's hot spot over the picture, 0..65535. Cheap: an atomic store, and a wake only on a change.
    pub fn set_cursor(&self, visible: bool, x: u16, y: u16) {
        let v = pack_cursor(visible, x, y);
        if self.shared.cursor_position.swap(v, Ordering::Relaxed) != v {
            self.shared.wake();
        }
    }

    /// A new pointer shape as a whole `CursorShape` message. It is also sent to every browser that connects later.
    pub fn set_cursor_shape(&self, message: Vec<u8>) {
        if let Ok(mut slot) = self.shared.cursor_shape.lock() {
            *slot = Some(message);
        }
        self.shared.wake();
    }
}

/// The local address a datagram arrived on. ICE matches it against our candidates, so `0.0.0.0` will not do.
/// A std socket cannot tell the real destination, so with a wildcard bind it is inferred from the source:
/// loopback for loopback senders, else the advertised address of the same family that is not loopback.
fn destination_for(local: SocketAddr, advertise: &[SocketAddr], source: SocketAddr) -> SocketAddr {
    if !local.ip().is_unspecified() {
        return local;
    }
    let loopback = source.ip().is_loopback();
    advertise
        .iter()
        .find(|a| a.ip().is_loopback() == loopback && a.is_ipv4() == source.is_ipv4())
        .copied()
        .unwrap_or(local)
}

/// Appends a frame; when the queue is over its size, the oldest frame comes back to be dropped.
fn push_bounded(queue: &mut VecDeque<VideoFrame>, frame: VideoFrame) -> Option<VideoFrame> {
    queue.push_back(frame);
    (queue.len() > QUEUE_FRAMES)
        .then(|| queue.pop_front())
        .flatten()
}

fn recycle(shared: &Shared, mut data: Vec<u8>) {
    data.clear();
    if let Ok(mut pool) = shared.pool.lock()
        && pool.len() < POOL_BUFFERS
    {
        pool.push(data);
    }
}

enum Command {
    Open {
        offer: String,
        reply: SyncSender<Result<String, String>>,
    },
    Close,
    Stop,
}

/// The media server: owns the network thread.
pub struct Server {
    commands: SyncSender<Command>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    local: SocketAddr,
}

impl Server {
    pub fn start(config: Config) -> std::io::Result<Self> {
        // The crypto backend chosen at build time (wincrypto, D-020) for every Rtc of this process.
        str0m::crypto::from_feature_flags().install_process_default();
        let socket = UdpSocket::bind(config.bind)?;
        let local = socket.local_addr()?;
        let waker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let shared = Arc::new(Shared {
            cursor_position: AtomicU64::new(0),
            cursor_shape: Mutex::new(None),
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_FRAMES + 1)),
            pool: Mutex::new(Vec::with_capacity(POOL_BUFFERS)),
            keyframe_wanted: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            counters: Counters::default(),
            wake_target: SocketAddr::from((Ipv4Addr::LOCALHOST, local.port())),
            waker,
        });
        let (commands, receiver) = sync_channel(8);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("webrtc".into())
            .spawn(move || NetworkLoop::new(socket, config, thread_shared, receiver).run())?;
        info!(%local, "media server started");
        Ok(Self {
            commands,
            shared,
            thread: Some(thread),
            local,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn sink(&self) -> FrameSink {
        FrameSink {
            shared: self.shared.clone(),
        }
    }

    /// Answers a browser offer. A new session replaces the previous one.
    pub fn open(&self, offer_sdp: String) -> Result<String, String> {
        let (reply, answer) = sync_channel(1);
        self.commands
            .send(Command::Open {
                offer: offer_sdp,
                reply,
            })
            .map_err(|_| "the media server stopped".to_string())?;
        self.shared.wake();
        answer
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "the media server did not answer".to_string())?
    }

    pub fn close(&self) {
        if self.commands.send(Command::Close).is_ok() {
            self.shared.wake();
        }
    }

    pub fn counters(&self) -> CountersSnapshot {
        let c = &self.shared.counters;
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        CountersSnapshot {
            sessions: load(&c.sessions),
            frames_sent: load(&c.frames_sent),
            bytes_sent: load(&c.bytes_sent),
            frames_dropped_queue: load(&c.frames_dropped_queue),
            frames_dropped_idle: load(&c.frames_dropped_idle),
            keyframe_requests: load(&c.keyframe_requests),
            write_errors: load(&c.write_errors),
            datagrams_dropped_test: load(&c.datagrams_dropped_test),
            input_messages: load(&c.input_messages),
            input_rejected: load(&c.input_rejected),
            input_dropped: load(&c.input_dropped),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.commands.send(Command::Stop).is_ok() {
            self.shared.wake();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Session {
    rtc: Rtc,
    video: Option<(Mid, Pt)>,
    connected: bool,
    /// RTP time 0 of the video stream, see `send_frames`.
    epoch: Instant,
    /// The same moment in Unix milliseconds, for the `hello` on the control channel.
    epoch_unix_ms: f64,
    control: Option<ChannelId>,
    input_fast: Option<ChannelId>,
    input_reliable: Option<ChannelId>,
    /// The session number given to input messages: the value of `Counters::sessions` when it opened.
    number: u64,
    /// The current pointer shape has gone out on `control`.
    shape_sent: bool,
    /// The last position sent on `input-fast`, `pack_cursor`.
    position_sent: u64,
    cursor_seq: u16,
}

fn unix_ms(at: SystemTime) -> f64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

struct NetworkLoop {
    socket: UdpSocket,
    config: Config,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    session: Option<Session>,
    /// The latest `CursorShape` message, for every session that connects.
    cursor_shape: Option<Vec<u8>>,
    /// Reused for `CursorState` messages.
    cursor_message: Vec<u8>,
    buffer: Vec<u8>,
    local: SocketAddr,
    wake_source: SocketAddr,
}

impl NetworkLoop {
    fn new(
        socket: UdpSocket,
        config: Config,
        shared: Arc<Shared>,
        commands: Receiver<Command>,
    ) -> Self {
        let local = socket.local_addr().unwrap_or(config.bind);
        let wake_source = shared.waker.local_addr().unwrap_or(local);
        Self {
            socket,
            config,
            shared,
            commands,
            session: None,
            cursor_shape: None,
            cursor_message: Vec::with_capacity(8),
            buffer: vec![0; RECV_BUFFER],
            local,
            wake_source,
        }
    }

    fn run(mut self) {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Open { offer, reply }) => {
                    let _ = reply.send(self.open(&offer));
                    continue;
                }
                Ok(Command::Close) => {
                    self.end_session("closed by the agent");
                    continue;
                }
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                    self.end_session("server stopped");
                    return;
                }
                Err(TryRecvError::Empty) => {}
            }
            self.send_frames();
            self.send_cursor();
            let deadline = self.drive();
            self.wait(deadline);
        }
    }

    fn open(&mut self, offer: &str) -> Result<String, String> {
        self.end_session("replaced by a new session");
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("bad offer: {e}"))?;
        let now = Instant::now();
        // playout-delay is not in the standard extension map; without it the zero delay never reaches the browser.
        let mut rtc = RtcConfig::new()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_h264(true)
            .set_extension(PLAYOUT_DELAY_ID, Extension::PlayoutDelay)
            // The capture time lets the browser report capture -> display latency (requestVideoFrameCallback).
            .set_extension(ABS_CAPTURE_TIME_ID, Extension::AbsoluteCaptureTime)
            .build(now);
        for addr in &self.config.advertise {
            match Candidate::host(*addr, "udp") {
                Ok(c) => {
                    if rtc.add_local_candidate(c).is_none() {
                        warn!(%addr, "candidate rejected");
                    }
                }
                Err(e) => warn!(%addr, error = %e, "bad candidate"),
            }
        }
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| format!("cannot accept the offer: {e}"))?;
        let number = self
            .shared
            .counters
            .sessions
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.session = Some(Session {
            rtc,
            video: None,
            connected: false,
            epoch: now,
            epoch_unix_ms: unix_ms(SystemTime::now()) - now.elapsed().as_secs_f64() * 1000.0,
            control: None,
            input_fast: None,
            input_reliable: None,
            number,
            shape_sent: false,
            position_sent: 0,
            cursor_seq: 0,
        });
        if let Some(link) = &self.config.input {
            link.live.store(number, Ordering::Relaxed);
        }
        info!("session opened");
        Ok(answer.to_sdp_string())
    }

    fn end_session(&mut self, why: &str) {
        if let Some(mut s) = self.session.take() {
            s.rtc.disconnect();
            info!(why, "session ended");
            if let Some(link) = &self.config.input {
                link.live.store(0, Ordering::Relaxed);
                link.release();
            }
        }
        self.shared.connected.store(false, Ordering::Relaxed);
    }

    /// Writes queued frames into the session, or drops them while nobody watches.
    fn send_frames(&mut self) {
        loop {
            let frame = match self.shared.queue.lock() {
                Ok(mut q) => q.pop_front(),
                Err(_) => None,
            };
            let Some(frame) = frame else { return };
            let ready = self
                .session
                .as_ref()
                .and_then(|s| s.connected.then_some(s.video).flatten());
            let counters = &self.shared.counters;
            match (self.session.as_mut(), ready) {
                (Some(session), Some((mid, pt))) => {
                    let rtp_time = MediaTime::from_90khz(
                        (frame
                            .captured
                            .saturating_duration_since(session.epoch)
                            .as_micros()
                            * 9
                            / 100) as u64,
                    );
                    let size = frame.data.len() as u64;
                    // Zero playout delay asks the browser to show frames as soon as they are decoded (Q-04).
                    // Wall-clock time of the capture: the same machine captures and sends, so the offset is zero.
                    let capture_time = SystemTime::now()
                        .checked_sub(frame.captured.elapsed())
                        .unwrap_or(SystemTime::now());
                    let written = session.rtc.writer(mid).map(|w| {
                        w.playout_delay(MediaTime::ZERO, MediaTime::ZERO)
                            .abs_capture_time(AbsCaptureTime {
                                capture_time,
                                clock_offset: Some(0),
                            })
                            .write(pt, frame.captured, rtp_time, frame.data.as_slice())
                    });
                    match written {
                        Some(Ok(())) => {
                            counters.frames_sent.fetch_add(1, Ordering::Relaxed);
                            counters.bytes_sent.fetch_add(size, Ordering::Relaxed);
                        }
                        Some(Err(e)) => {
                            counters.write_errors.fetch_add(1, Ordering::Relaxed);
                            debug!(error = %e, "frame not written");
                        }
                        None => {
                            counters.write_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                _ => {
                    counters.frames_dropped_idle.fetch_add(1, Ordering::Relaxed);
                }
            }
            recycle(&self.shared, frame.data);
        }
    }

    /// Sends a new pointer shape on `control` and a new position on `input-fast`, once each channel is open.
    fn send_cursor(&mut self) {
        if let Some(shape) = self
            .shared
            .cursor_shape
            .lock()
            .ok()
            .and_then(|mut s| s.take())
        {
            self.cursor_shape = Some(shape);
            if let Some(s) = self.session.as_mut() {
                s.shape_sent = false;
            }
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if !session.shape_sent
            && let (Some(id), Some(shape)) = (session.control, self.cursor_shape.as_ref())
        {
            session.shape_sent = true;
            if let Some(mut channel) = session.rtc.channel(id)
                && let Err(e) = channel.write(true, shape)
            {
                debug!(error = %e, "cursor shape not sent");
            }
        }
        let position = self.shared.cursor_position.load(Ordering::Relaxed);
        if position != session.position_sent
            && let (Some(id), Some((visible, x, y))) = (session.input_fast, unpack_cursor(position))
        {
            session.position_sent = position;
            session.cursor_seq = session.cursor_seq.wrapping_add(1);
            self.cursor_message.clear();
            ServerMessage::CursorState {
                seq: session.cursor_seq,
                visible,
                x,
                y,
            }
            .encode(&mut self.cursor_message);
            if let Some(mut channel) = session.rtc.channel(id)
                && let Err(e) = channel.write(true, &self.cursor_message)
            {
                debug!(error = %e, "cursor position not sent");
            }
        }
    }

    /// Sends what the session has to send and handles its events. Returns when it next needs a timeout.
    fn drive(&mut self) -> Instant {
        let idle = Instant::now() + IDLE_WAIT;
        let Some(session) = self.session.as_mut() else {
            return idle;
        };
        let mut ended = None;
        let deadline = loop {
            match session.rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t.min(idle),
                Ok(Output::Transmit(t)) => {
                    // Only once media flows, so ICE and DTLS are never part of the simulated outage.
                    if session.connected
                        && let Some(test) = self.config.simulate_loss
                        && test.drops(session.epoch.elapsed())
                    {
                        self.shared
                            .counters
                            .datagrams_dropped_test
                            .fetch_add(1, Ordering::Relaxed);
                    } else if let Err(e) = self.socket.send_to(&t.contents, t.destination) {
                        debug!(error = %e, to = %t.destination, "send failed");
                    }
                }
                Ok(Output::Event(event)) => {
                    if let Some(why) =
                        handle_event(session, &self.shared, self.config.input.as_ref(), event)
                    {
                        ended = Some(why);
                    }
                }
                Err(e) => {
                    ended = Some("rtc error");
                    warn!(error = %e, "rtc failed");
                    break idle;
                }
            }
        };
        if let Some(why) = ended {
            self.end_session(why);
        }
        deadline
    }

    /// Waits for a datagram or the deadline and feeds the session.
    fn wait(&mut self, deadline: Instant) {
        let now = Instant::now();
        let timeout = deadline
            .saturating_duration_since(now)
            .max(Duration::from_millis(1));
        if self.socket.set_read_timeout(Some(timeout)).is_err() {
            return;
        }
        match self.socket.recv_from(&mut self.buffer) {
            Ok((n, source)) => {
                if source == self.wake_source && n == 1 {
                    return;
                }
                let Some(session) = self.session.as_mut() else {
                    return;
                };
                let Ok(contents) = self.buffer[..n].try_into() else {
                    return;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: destination_for(self.local, &self.config.advertise, source),
                        contents,
                    },
                );
                if session.rtc.accepts(&input)
                    && let Err(e) = session.rtc.handle_input(input)
                {
                    debug!(error = %e, "input rejected");
                }
            }
            // A browser that went away makes Windows report the ICMP error on the next receive.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if let Some(session) = self.session.as_mut()
                    && let Err(e) = session.rtc.handle_input(Input::Timeout(Instant::now()))
                {
                    debug!(error = %e, "timeout rejected");
                }
            }
            Err(e) => warn!(error = %e, "receive failed"),
        }
    }
}

/// Returns why the session ended, if it did.
fn handle_event(
    session: &mut Session,
    shared: &Shared,
    input: Option<&InputLink>,
    event: Event,
) -> Option<&'static str> {
    match event {
        Event::MediaAdded(added) if added.kind == MediaKind::Video => {
            let pt = session
                .rtc
                .writer(added.mid)
                .and_then(|w| choose_h264(w.payload_params().map(|p| (p.pt(), p.spec()))));
            match pt {
                Some((pt, level_id)) => {
                    let profile_level_id = level_id.map(|v| format!("{v:06x}")).unwrap_or_default();
                    info!(mid = %added.mid, ?pt, %profile_level_id, "video track ready");
                    session.video = Some((added.mid, pt));
                }
                None => warn!("the browser offered no usable H.264"),
            }
        }
        Event::IceConnectionStateChange(state) => {
            info!(?state, "ice state");
            match state {
                IceConnectionState::Connected | IceConnectionState::Completed => {}
                IceConnectionState::Disconnected => return Some("ice disconnected"),
                _ => {}
            }
        }
        Event::Connected => {
            info!("dtls connected");
            session.connected = true;
            shared.connected.store(true, Ordering::Relaxed);
            // The first frame the browser sees must be an IDR.
            shared.keyframe_wanted.store(true, Ordering::Relaxed);
        }
        Event::KeyframeRequest(request) => {
            debug!(kind = ?request.kind, "keyframe requested");
            shared
                .counters
                .keyframe_requests
                .fetch_add(1, Ordering::Relaxed);
            shared.keyframe_wanted.store(true, Ordering::Relaxed);
        }
        Event::ChannelOpen(id, label) => {
            info!(%label, "data channel open");
            if label == INPUT_FAST_CHANNEL {
                session.input_fast = Some(id);
                // The browser draws the pointer from the next position sent.
                session.position_sent = 0;
            } else if label == INPUT_RELIABLE_CHANNEL {
                session.input_reliable = Some(id);
            } else if label == CONTROL_CHANNEL {
                session.control = Some(id);
                session.shape_sent = false;
                let hello = json!({
                    "t": "hello",
                    "protocol": 1,
                    "codec": "h264",
                    "rtp_epoch_ms": session.epoch_unix_ms,
                    "rtp_clock": 90_000,
                });
                send_control(session, &hello);
            }
        }
        Event::ChannelData(data) if Some(data.id) == session.control && !data.binary => {
            if let Some(reply) = control_reply(&data.data, unix_ms(SystemTime::now())) {
                send_control(session, &reply);
            }
        }
        Event::ChannelData(data)
            if Some(data.id) == session.input_fast || Some(data.id) == session.input_reliable =>
        {
            let parsed = if data.binary {
                ClientMessage::parse(&data.data)
            } else {
                Err(ParseError::Malformed(0))
            };
            pass_input(shared, input, session.number, parsed);
        }
        Event::ChannelClose(id) => {
            if Some(id) == session.control {
                session.control = None;
            }
            if Some(id) == session.input_fast || Some(id) == session.input_reliable {
                if Some(id) == session.input_fast {
                    session.input_fast = None;
                } else {
                    session.input_reliable = None;
                }
                if let Some(link) = input {
                    link.release();
                }
            }
            debug!("data channel closed");
        }
        _ => {}
    }
    None
}

/// Labels of the data channels, `docs/PROTOCOL.md`.
const CONTROL_CHANNEL: &str = "control";
const INPUT_FAST_CHANNEL: &str = "input-fast";
const INPUT_RELIABLE_CHANNEL: &str = "input-reliable";

/// Hands a parsed input message to the input thread without waiting, or counts why it could not.
fn pass_input(
    shared: &Shared,
    input: Option<&InputLink>,
    session: u64,
    parsed: Result<ClientMessage, ParseError>,
) {
    let counters = &shared.counters;
    let message = match parsed {
        Ok(m) => m,
        Err(e) => {
            counters.input_rejected.fetch_add(1, Ordering::Relaxed);
            debug!(error = %e, "input message rejected");
            return;
        }
    };
    let Some(link) = input else {
        counters.input_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    match link.sender.try_send(InputMessage { session, message }) {
        Ok(()) => {
            counters.input_messages.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            // A lost key release would leave the key down: let go of everything instead.
            counters.input_dropped.fetch_add(1, Ordering::Relaxed);
            link.release();
        }
    }
}

fn send_control(session: &mut Session, message: &Value) {
    let Some(id) = session.control else { return };
    if let Some(mut channel) = session.rtc.channel(id)
        && let Err(e) = channel.write(false, message.to_string().as_bytes())
    {
        debug!(error = %e, "control write failed");
    }
}

/// The answer to a control message, if it needs one. Phase 2 answers `ping` with the server clock.
fn control_reply(text: &[u8], server_ms: f64) -> Option<Value> {
    let message: Value = serde_json::from_slice(text).ok()?;
    match message["t"].as_str()? {
        "ping" => Some(json!({
            "t": "pong",
            "id": message["id"],
            "client_ts": message["client_ts"],
            "server_ms": server_ms,
        })),
        _ => None,
    }
}

/// The H.264 payload type to send with: packetization-mode 1, High profile when offered, else any.
/// Also returns its `profile-level-id`, for the log.
fn choose_h264(
    params: impl Iterator<Item = (Pt, str0m::format::CodecSpec)>,
) -> Option<(Pt, Option<u32>)> {
    let mut best: Option<(u8, Pt, Option<u32>)> = None;
    for (pt, spec) in params {
        if spec.codec != Codec::H264 || spec.format.packetization_mode != Some(1) {
            continue;
        }
        let level_id = spec.format.profile_level_id;
        let rank = match level_id.map(|v| (v >> 16) as u8) {
            Some(0x64) => 3,
            Some(0x4d) => 2,
            Some(_) => 1,
            None => 0,
        };
        if best.is_none_or(|(r, _, _)| rank > r) {
            best = Some((rank, pt, level_id));
        }
    }
    best.map(|(_, pt, level_id)| (pt, level_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> VideoFrame {
        VideoFrame {
            data: vec![tag],
            captured: Instant::now(),
            keyframe: false,
        }
    }

    #[test]
    fn ping_gets_a_pong_with_the_server_clock() {
        let reply = control_reply(br#"{"t":"ping","id":7,"client_ts":12.5}"#, 1000.25).unwrap();
        assert_eq!(reply["t"], "pong");
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["client_ts"], 12.5);
        assert_eq!(reply["server_ms"], 1000.25);
        assert!(control_reply(br#"{"t":"settings"}"#, 0.0).is_none());
        assert!(control_reply(b"not json", 0.0).is_none());
    }

    #[test]
    fn cursor_positions_pack_and_unpack() {
        assert_eq!(unpack_cursor(0), None, "nothing set yet");
        for (v, x, y) in [(true, 0, 0), (false, 65535, 1), (true, 1234, 65535)] {
            assert_eq!(unpack_cursor(pack_cursor(v, x, y)), Some((v, x, y)));
        }
        assert_ne!(pack_cursor(false, 0, 0), 0);
    }

    fn shared() -> Shared {
        Shared {
            cursor_position: AtomicU64::new(0),
            cursor_shape: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            pool: Mutex::new(Vec::new()),
            keyframe_wanted: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            counters: Counters::default(),
            waker: UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap(),
            wake_target: SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
        }
    }

    #[test]
    fn input_goes_to_the_queue_and_a_full_queue_asks_for_release() {
        let shared = shared();
        let (sender, receiver) = sync_channel(1);
        let link = InputLink {
            sender,
            release: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicU64::new(4)),
        };
        let key = ClientMessage::Key {
            down: true,
            scancode: 0x1e,
        };
        pass_input(&shared, Some(&link), 4, Ok(key.clone()));
        pass_input(&shared, Some(&link), 4, Ok(ClientMessage::ReleaseAll));
        pass_input(&shared, Some(&link), 4, Err(ParseError::Unknown(0x7f)));
        let got = receiver.try_recv().unwrap();
        assert_eq!((got.session, got.message), (4, key));
        assert!(
            receiver.try_recv().is_err(),
            "the second message did not fit"
        );
        assert!(link.release.load(Ordering::Relaxed));
        let c = &shared.counters;
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        assert_eq!(
            (
                load(&c.input_messages),
                load(&c.input_dropped),
                load(&c.input_rejected)
            ),
            (1, 1, 1)
        );
        pass_input(&shared, None, 4, Ok(ClientMessage::ReleaseAll));
        assert_eq!(load(&c.input_dropped), 2, "no input thread");
    }

    #[test]
    fn loss_test_drops_only_inside_the_burst() {
        let test = LossTest {
            every: Duration::from_secs(5),
            burst: Duration::from_millis(300),
        };
        assert!(test.drops(Duration::from_millis(100)));
        assert!(!test.drops(Duration::from_millis(400)));
        assert!(test.drops(Duration::from_millis(5_200)));
        assert!(!test.drops(Duration::from_millis(9_999)));
    }

    #[test]
    fn overflow_drops_the_oldest_frame() {
        let mut queue = VecDeque::new();
        assert!(push_bounded(&mut queue, frame(1)).is_none());
        assert!(push_bounded(&mut queue, frame(2)).is_none());
        let dropped = push_bounded(&mut queue, frame(3)).expect("one frame over the limit");
        assert_eq!(dropped.data, [1]);
        let left: Vec<u8> = queue.iter().map(|f| f.data[0]).collect();
        assert_eq!(left, [2, 3]);
    }

    #[test]
    fn wildcard_binds_infer_the_destination_from_the_source() {
        let local: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let lan: SocketAddr = "192.168.1.12:8443".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:8443".parse().unwrap();
        let advertise = [lan, lo];
        let phone: SocketAddr = "192.168.1.40:51000".parse().unwrap();
        let me: SocketAddr = "127.0.0.1:51000".parse().unwrap();
        assert_eq!(destination_for(local, &advertise, phone), lan);
        assert_eq!(destination_for(local, &advertise, me), lo);
        assert_eq!(destination_for(lo, &advertise, phone), lo);
    }

    #[test]
    fn server_answers_nothing_without_an_offer_and_stops() {
        let server = Server::start(Config {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            advertise: Vec::new(),
            simulate_loss: None,
            input: None,
        })
        .expect("bind on loopback");
        assert!(server.open("not an sdp".into()).is_err());
        let sink = server.sink();
        assert!(!sink.connected());
        sink.push(frame(1));
        drop(server);
    }
}
