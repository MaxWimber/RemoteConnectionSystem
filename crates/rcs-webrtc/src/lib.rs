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
//!
//! Sound goes the same way as video (D-038): the audio thread hands Opus packets through [`FrameSink::push_audio`],
//! a queue of eight packets that drops the oldest when full.
//!
//! The `clipboard` channel (D-040) carries text both ways in parts: texts from the browser go to the agent's
//! clipboard thread through [`ClipboardLink`], texts from the clipboard thread wait in a short queue here.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcs_protocol::clipboard::{self, Assembler};
use rcs_protocol::input::{ClientMessage, ParseError, ServerMessage};
use serde_json::{Value, json};
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{AbsCaptureTime, Extension};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tracing::{debug, info, warn};

mod link;
mod mode;
mod rate;
mod timing;

pub use link::LinkTest;
use link::Shaper;
use mode::ModeControl;
pub use mode::StreamMode;
pub use rate::{RateControl, RateLimits};
use timing::FrameTimings;

/// Frames waiting for the network thread. Two is enough to ride out one slow send without adding latency.
const QUEUE_FRAMES: usize = 2;
/// Spare frame buffers kept for the encoder.
const POOL_BUFFERS: usize = 4;
/// Opus packets waiting for the network thread: 80 ms (D-038).
const AUDIO_QUEUE: usize = 8;
/// The payload type offered for Opus by Chrome and Edge; the answer follows the offer anyway.
const OPUS_PT: u8 = 111;
/// Longest wait of the network thread when nothing is scheduled.
const IDLE_WAIT: Duration = Duration::from_millis(50);
/// Timeouts handled at once in one pass of the loop, so frames and commands still get their turn.
const DUE_NOW_LIMIT: u32 = 4096;
const WAKE: [u8; 1] = [0x5a];
/// Free in the standard map of str0m; the answer follows the ids of the offer anyway.
const PLAYOUT_DELAY_ID: u8 = 6;
const ABS_CAPTURE_TIME_ID: u8 = 7;
/// Largest datagram we expect; ICE, DTLS and RTCP from browsers fit easily.
const RECV_BUFFER: usize = 2048;
/// How often `str0m` sums up the receiver reports: loss and RTT for the rate control.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

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
    /// The encoder's bitrate range; the start value is used as is when `adaptive` is off.
    pub rate: RateLimits,
    /// Follow the send-side estimate (D-027).
    pub adaptive: bool,
    /// Hold video to the pacer's rate, 1.1 x the estimate. Off: every frame goes out at once, the estimate still
    /// comes from TWCC (P8-5).
    pub paced_video: bool,
    /// Testing only (P8-1): a narrow link on the way out, see [`LinkTest`].
    pub simulate_link: Option<LinkTest>,
    /// Where texts from the browser's clipboard go (D-040). Without it the `clipboard` channel is ignored.
    pub clipboard: Option<ClipboardLink>,
}

/// The network thread's end of the clipboard (D-040).
#[derive(Debug, Clone)]
pub struct ClipboardLink {
    /// Whole texts from the browser, for the clipboard thread.
    pub incoming: SyncSender<String>,
    /// Set while a browser has its `clipboard` channel open: only then is the Windows clipboard read.
    pub enabled: Arc<AtomicBool>,
}

/// What the clipboard thread has for the browser.
#[derive(Debug)]
enum ClipboardOut {
    Text(Vec<u8>),
    Applied(u32),
    TooLarge(u32),
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

/// One Opus packet of 10 ms.
#[derive(Debug)]
pub struct AudioPacket {
    pub data: Vec<u8>,
    /// When its first sample went through the Windows mixer; the RTP timestamp is derived from it.
    pub captured: Instant,
}

/// Counters for logs and the statistics panel.
#[derive(Debug, Default)]
pub struct Counters {
    pub sessions: AtomicU64,
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Datagram bytes `str0m` let out of its pacer: media with retransmissions, padding and RTCP. Read against
    /// `bytes_sent` it shows whether a queue grows inside the session.
    pub bytes_out: AtomicU64,
    /// Dropped because the queue was full: the network thread fell behind.
    pub frames_dropped_queue: AtomicU64,
    /// Dropped because no browser was connected yet.
    pub frames_dropped_idle: AtomicU64,
    /// PLI and FIR from the browser.
    pub keyframe_requests: AtomicU64,
    pub write_errors: AtomicU64,
    /// Datagrams held back by `Config::simulate_loss`, or dropped by the full queue of `Config::simulate_link`.
    pub datagrams_dropped_test: AtomicU64,
    /// Changes of the encoder's target bitrate.
    pub rate_changes: AtomicU64,
    /// Input messages handed to the input thread.
    pub input_messages: AtomicU64,
    /// Input messages of an unknown type, or malformed: wrong length, a field out of range, text on a binary channel.
    pub input_rejected: AtomicU64,
    /// Input messages dropped because the input queue was full.
    pub input_dropped: AtomicU64,
    pub audio_packets_sent: AtomicU64,
    pub audio_bytes_sent: AtomicU64,
    /// Opus packets dropped because the audio queue was full, or nobody was connected.
    pub audio_dropped: AtomicU64,
    /// Whole texts received from the browser and sent to it; their content is never logged.
    pub clipboard_in: AtomicU64,
    pub clipboard_out: AtomicU64,
    /// Clipboard messages refused: malformed, out of order, not UTF-8, or a full queue.
    pub clipboard_rejected: AtomicU64,
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
    pub rate_changes: u64,
    pub input_messages: u64,
    pub input_rejected: u64,
    pub input_dropped: u64,
    pub audio_packets_sent: u64,
    pub audio_bytes_sent: u64,
    pub audio_dropped: u64,
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
    audio_queue: Mutex<VecDeque<AudioPacket>>,
    audio_pool: Mutex<Vec<Vec<u8>>>,
    clipboard_out: Mutex<VecDeque<ClipboardOut>>,
    keyframe_wanted: AtomicBool,
    connected: AtomicBool,
    /// The active `StreamMode`, for the encoder thread (D-030).
    mode: AtomicU8,
    counters: Counters,
    waker: UdpSocket,
    wake_target: SocketAddr,
    rate: RateShared,
}

/// What the rate control decided, for the encoder thread and the statistics; 0 means not known yet.
#[derive(Debug, Default)]
pub struct RateShared {
    target_bps: AtomicU32,
    estimate_bps: AtomicU32,
    loss_permille: AtomicU32,
    rtt_us: AtomicU32,
}

/// A copy of [`RateShared`] for statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RateSnapshot {
    pub target_bps: u32,
    pub estimate_bps: Option<u32>,
    pub loss: Option<f32>,
    pub rtt: Option<Duration>,
}

impl RateShared {
    fn publish(&self, rate: &RateControl) {
        // 0 stands for «not known yet»: loss and RTT are stored one higher, an estimate of 0 bit/s never comes.
        self.target_bps.store(rate.target(), Ordering::Relaxed);
        self.estimate_bps
            .store(rate.estimate().unwrap_or(0), Ordering::Relaxed);
        self.loss_permille.store(
            rate.loss()
                .map_or(0, |l| (l.clamp(0.0, 1.0) * 1000.0) as u32 + 1),
            Ordering::Relaxed,
        );
        self.rtt_us.store(
            rate.rtt().map_or(0, |r| {
                r.as_micros().min(u128::from(u32::MAX - 1)) as u32 + 1
            }),
            Ordering::Relaxed,
        );
    }

    fn snapshot(&self) -> RateSnapshot {
        let load = |v: &AtomicU32| v.load(Ordering::Relaxed);
        let some = |v: u32| (v > 0).then_some(v);
        RateSnapshot {
            target_bps: load(&self.target_bps),
            estimate_bps: some(load(&self.estimate_bps)),
            loss: some(load(&self.loss_permille)).map(|v| (v - 1) as f32 / 1000.0),
            rtt: some(load(&self.rtt_us)).map(|v| Duration::from_micros(u64::from(v - 1))),
        }
    }
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

    /// An empty buffer for the next Opus packet, reused from packets already sent.
    pub fn audio_buffer(&self) -> Vec<u8> {
        self.shared
            .audio_pool
            .lock()
            .ok()
            .and_then(|mut p| p.pop())
            .unwrap_or_default()
    }

    /// Queues an Opus packet; when the queue is full the oldest packet is dropped.
    pub fn push_audio(&self, packet: AudioPacket) {
        let old = self.shared.audio_queue.lock().ok().and_then(|mut queue| {
            queue.push_back(packet);
            (queue.len() > AUDIO_QUEUE)
                .then(|| queue.pop_front())
                .flatten()
        });
        if let Some(old) = old {
            self.shared
                .counters
                .audio_dropped
                .fetch_add(1, Ordering::Relaxed);
            recycle_audio(&self.shared, old.data);
        }
        self.shared.wake();
    }

    /// A new text from the Windows clipboard for the browser; an older one not yet sent is replaced.
    pub fn send_clipboard(&self, text: Vec<u8>) {
        self.queue_clipboard(ClipboardOut::Text(text));
    }

    /// The browser's text of `total` bytes is now in the Windows clipboard.
    pub fn clipboard_applied(&self, total: u32) {
        self.queue_clipboard(ClipboardOut::Applied(total));
    }

    /// The Windows clipboard holds a text over the limit; the browser is told its size instead.
    pub fn clipboard_too_large(&self, size: u32) {
        self.queue_clipboard(ClipboardOut::TooLarge(size));
    }

    fn queue_clipboard(&self, item: ClipboardOut) {
        if let Ok(mut queue) = self.shared.clipboard_out.lock() {
            if matches!(item, ClipboardOut::Text(_) | ClipboardOut::TooLarge(_)) {
                queue.retain(|q| matches!(q, ClipboardOut::Applied(_)));
            }
            queue.push_back(item);
            while queue.len() > 4 {
                queue.pop_front();
            }
        }
        self.shared.wake();
    }

    /// True once per request: the browser asked for a keyframe, a session started, or frames were dropped.
    pub fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe_wanted.swap(false, Ordering::Relaxed)
    }

    /// The stream mode the encoder should use now (D-030). Cheap: one atomic load.
    pub fn stream_mode(&self) -> StreamMode {
        StreamMode::from_u8(self.shared.mode.load(Ordering::Relaxed))
    }

    /// The bitrate the encoder should use now (D-027). Cheap: one atomic load.
    pub fn target_bitrate(&self) -> u32 {
        self.shared.rate.target_bps.load(Ordering::Relaxed)
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

fn recycle_audio(shared: &Shared, mut data: Vec<u8>) {
    data.clear();
    if let Ok(mut pool) = shared.audio_pool.lock()
        && pool.len() < AUDIO_QUEUE * 2
    {
        pool.push(data);
    }
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
            audio_queue: Mutex::new(VecDeque::with_capacity(AUDIO_QUEUE + 1)),
            audio_pool: Mutex::new(Vec::with_capacity(AUDIO_QUEUE * 2)),
            clipboard_out: Mutex::new(VecDeque::new()),
            keyframe_wanted: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            mode: AtomicU8::new(StreamMode::Game.to_u8()),
            counters: Counters::default(),
            wake_target: SocketAddr::from((Ipv4Addr::LOCALHOST, local.port())),
            waker,
            rate: RateShared::default(),
        });
        shared.rate.publish(&RateControl::new(config.rate));
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

    pub fn rate(&self) -> RateSnapshot {
        self.shared.rate.snapshot()
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
            rate_changes: load(&c.rate_changes),
            input_messages: load(&c.input_messages),
            input_rejected: load(&c.input_rejected),
            input_dropped: load(&c.input_dropped),
            audio_packets_sent: load(&c.audio_packets_sent),
            audio_bytes_sent: load(&c.audio_bytes_sent),
            audio_dropped: load(&c.audio_dropped),
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
    /// The Opus track, when the browser offered one (D-038).
    audio: Option<(Mid, Pt)>,
    /// The `clipboard` channel and the text it is receiving (D-040).
    clipboard: Option<ChannelId>,
    clipboard_in: Assembler,
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
    rate: RateControl,
    /// The client's mode and the one streaming now (D-030).
    mode: ModeControl,
    /// The client's bitrate limit, inside the server's range (D-029).
    client_max: Option<u32>,
    /// `Config::simulate_link`, from the moment media flows.
    shaper: Option<Shaper>,
    /// Capture to `str0m` and capture to the last packet out, summed up every `TIMING_EVERY` (P8-5).
    timings: FrameTimings,
    timings_since: Instant,
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
    /// The last once-a-second rate line: when, bytes sent, datagram bytes out, test drops and sound bytes by then.
    sampled: (Instant, u64, u64, u64, u64),
}

/// How often a connected session logs its rate: estimate, target and what actually went out.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How often a connected session logs where its frames' time went (P8-5).
const TIMING_EVERY: Duration = Duration::from_secs(10);

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
            sampled: (Instant::now(), 0, 0, 0, 0),
        }
    }

    /// The rate control climbs on the clock too: `str0m` reports an estimate only when it changes (D-027).
    fn tick_rate(&mut self) {
        if !self.config.adaptive {
            return;
        }
        if let Some(session) = self.session.as_mut().filter(|s| s.connected) {
            let before = session.rate.target();
            session.rate.on_tick(Instant::now());
            if session.rate.target() != before {
                rate_changed(session, &self.shared, before, true);
            }
        }
    }

    /// One line a second while media flows, so a run can be read back from the log (P8-1).
    fn sample(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.sampled.0);
        if elapsed < SAMPLE_EVERY {
            return;
        }
        let c = &self.shared.counters;
        let bytes = c.bytes_sent.load(Ordering::Relaxed);
        let out = c.bytes_out.load(Ordering::Relaxed);
        let dropped = c.datagrams_dropped_test.load(Ordering::Relaxed);
        let audio = c.audio_bytes_sent.load(Ordering::Relaxed);
        let (_, last_bytes, last_out, last_dropped, last_audio) =
            std::mem::replace(&mut self.sampled, (now, bytes, out, dropped, audio));
        let kbps = |b: u64| (b as f64 * 8.0 / elapsed.as_secs_f64() / 1000.0) as u64;
        let Some(session) = self.session.as_mut().filter(|s| s.connected) else {
            return;
        };
        let r = self.shared.rate.snapshot();
        let sent_kbps = kbps(bytes.saturating_sub(last_bytes));
        let out_kbps = kbps(out.saturating_sub(last_out));
        let audio_kbps = kbps(audio.saturating_sub(last_audio));
        let rtt_ms = r.rtt.map(|d| (d.as_secs_f64() * 10_000.0).round() / 10.0);
        info!(
            target_kbps = r.target_bps / 1000,
            estimate_kbps = r.estimate_bps.map(|v| v / 1000),
            sent_kbps,
            out_kbps,
            audio_kbps,
            dropped_test = dropped.saturating_sub(last_dropped),
            loss = r.loss,
            rtt_ms,
            "rate sample"
        );
        if now.duration_since(session.timings_since) >= TIMING_EVERY {
            session.timings_since = now;
            let (written, out) = session.timings.take();
            let ms = |s: Option<timing::Spread>| s.map(|s| [s.p50, s.p95, s.p99, s.max]);
            info!(
                written_ms = ?ms(written),
                out_ms = ?ms(out),
                paced = self.config.paced_video,
                "frame timing"
            );
        }
        // «Слабая сеть» by itself when the target stays low, back when the link has room (D-030).
        let adaptive = self.config.adaptive;
        if adaptive {
            let cap =
                session_limits(self.config.rate, session.mode.chosen(), session.client_max).max_bps;
            if session
                .mode
                .on_rate(now, r.target_bps, r.estimate_bps, r.loss, cap)
            {
                apply_mode(session, &self.shared, &self.config);
            }
        }
        // The same numbers for the statistics panel (D-029).
        let limits = session.rate.limits();
        let target = if adaptive {
            r.target_bps
        } else {
            self.config.rate.start_bps
        };
        let stats = json!({
            "t": "stats",
            "adaptive": adaptive,
            "target_kbps": target / 1000,
            "estimate_kbps": r.estimate_bps.map(|v| v / 1000),
            "sent_kbps": sent_kbps,
            "out_kbps": out_kbps,
            "audio_kbps": audio_kbps,
            "loss": r.loss,
            "rtt_ms": rtt_ms,
            "min_kbps": limits.min_bps / 1000,
            "max_kbps": limits.max_bps / 1000,
            "rate_changes": c.rate_changes.load(Ordering::Relaxed),
            "mode": session.mode.chosen().name(),
            "active_mode": session.mode.active().name(),
        });
        send_control(session, &stats);
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
            self.send_audio();
            self.send_clipboard();
            self.send_cursor();
            self.tick_rate();
            self.sample();
            let deadline = self.drive();
            self.wait(deadline);
        }
    }

    fn open(&mut self, offer: &str) -> Result<String, String> {
        self.end_session("replaced by a new session");
        let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("bad offer: {e}"))?;
        let now = Instant::now();
        // playout-delay is not in the standard extension map; without it the zero delay never reaches the browser.
        let mut config = RtcConfig::new()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_h264(true);
        // Stereo in both directions of the fmtp: Chrome decodes mono otherwise (D-038).
        config.codec_config().add_config(
            OPUS_PT.into(),
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                min_p_time: Some(10),
                use_inband_fec: Some(true),
                stereo: Some(true),
                sprop_stereo: Some(true),
                ..Default::default()
            },
        );
        let mut rtc = config
            .set_extension(PLAYOUT_DELAY_ID, Extension::PlayoutDelay)
            // The capture time lets the browser report capture -> display latency (requestVideoFrameCallback).
            .set_extension(ABS_CAPTURE_TIME_ID, Extension::AbsoluteCaptureTime)
            // Loss and RTT from the receiver reports reach the rate control as `MediaEgressStats`.
            .set_stats_interval(Some(STATS_INTERVAL))
            .enable_bwe(
                self.config
                    .adaptive
                    .then(|| Bitrate::bps(u64::from(self.config.rate.start_bps))),
            )
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
        // The mode streaming before goes on until the client sends its own: no second new encoder on reconnect.
        let last_mode = StreamMode::from_u8(self.shared.mode.load(Ordering::Relaxed));
        self.session = Some(Session {
            rtc,
            video: None,
            audio: None,
            clipboard: None,
            clipboard_in: Assembler::default(),
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
            rate: RateControl::new(session_limits(self.config.rate, last_mode, None)),
            mode: ModeControl::new(now, last_mode),
            client_max: None,
            shaper: None,
            timings: FrameTimings::default(),
            timings_since: now,
        });
        self.shared.rate.publish(&RateControl::new(session_limits(
            self.config.rate,
            last_mode,
            None,
        )));
        if let Some(link) = &self.config.input {
            link.live.store(number, Ordering::Relaxed);
        }
        info!("session opened");
        Ok(answer.to_sdp_string())
    }

    fn end_session(&mut self, why: &str) {
        if let Some(link) = &self.config.clipboard {
            link.enabled.store(false, Ordering::Relaxed);
        }
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
                            session.timings.written(frame.captured.elapsed());
                            counters.frames_sent.fetch_add(1, Ordering::Relaxed);
                            session.rate.on_sent(Instant::now(), size as usize);
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

    /// Writes queued Opus packets into the session, or drops them while nobody listens.
    fn send_audio(&mut self) {
        loop {
            let packet = match self.shared.audio_queue.lock() {
                Ok(mut q) => q.pop_front(),
                Err(_) => None,
            };
            let Some(packet) = packet else { return };
            let counters = &self.shared.counters;
            let track = self
                .session
                .as_mut()
                .filter(|s| s.connected)
                .and_then(|s| s.audio.map(|a| (s, a)));
            match track {
                Some((session, (mid, pt))) => {
                    let ticks = packet
                        .captured
                        .saturating_duration_since(session.epoch)
                        .as_micros()
                        * 48
                        / 1000;
                    let rtp_time = MediaTime::new(ticks as u64, Frequency::FORTY_EIGHT_KHZ);
                    let capture_time = SystemTime::now()
                        .checked_sub(packet.captured.elapsed())
                        .unwrap_or(SystemTime::now());
                    let size = packet.data.len() as u64;
                    let written = session.rtc.writer(mid).map(|w| {
                        w.abs_capture_time(AbsCaptureTime {
                            capture_time,
                            clock_offset: Some(0),
                        })
                        .write(
                            pt,
                            packet.captured,
                            rtp_time,
                            packet.data.as_slice(),
                        )
                    });
                    match written {
                        Some(Ok(())) => {
                            counters.audio_packets_sent.fetch_add(1, Ordering::Relaxed);
                            counters.audio_bytes_sent.fetch_add(size, Ordering::Relaxed);
                        }
                        Some(Err(e)) => {
                            counters.write_errors.fetch_add(1, Ordering::Relaxed);
                            debug!(error = %e, "audio not written");
                        }
                        None => {
                            counters.write_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                None => {
                    counters.audio_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            recycle_audio(&self.shared, packet.data);
        }
    }

    /// Sends what the clipboard thread queued, once the browser's `clipboard` channel is open.
    fn send_clipboard(&mut self) {
        let Some((session, id)) = self
            .session
            .as_mut()
            .and_then(|s| s.clipboard.map(|id| (s, id)))
        else {
            return;
        };
        loop {
            let item = self
                .shared
                .clipboard_out
                .lock()
                .ok()
                .and_then(|mut q| q.pop_front());
            let Some(item) = item else { return };
            let messages = match item {
                ClipboardOut::Text(text) => match clipboard::encode_text(&text) {
                    Ok(m) => {
                        info!(
                            bytes = text.len(),
                            parts = m.len(),
                            "clipboard to the browser"
                        );
                        self.shared
                            .counters
                            .clipboard_out
                            .fetch_add(1, Ordering::Relaxed);
                        m
                    }
                    Err(_) => vec![clipboard::encode_too_large(text.len() as u32)],
                },
                ClipboardOut::Applied(total) => vec![clipboard::encode_applied(total)],
                ClipboardOut::TooLarge(size) => vec![clipboard::encode_too_large(size)],
            };
            let Some(mut channel) = session.rtc.channel(id) else {
                return;
            };
            for m in messages {
                if let Err(e) = channel.write(true, &m) {
                    debug!(error = %e, "clipboard message not sent");
                    break;
                }
            }
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
        let mut due_now = 0;
        let deadline = loop {
            match session.rtc.poll_output() {
                // The pacer asks for a timeout right away after every packet it lets out. Sleeping first, even for
                // the shortest socket timeout of 1 ms, let out one packet per millisecond: about 8 Mbit/s, the rest
                // waited in its queue for up to 2 s (P8-1, `bwe-mf2`).
                Ok(Output::Timeout(t)) if due_now < DUE_NOW_LIMIT && t <= Instant::now() => {
                    due_now += 1;
                    if let Err(e) = session.rtc.handle_input(Input::Timeout(Instant::now())) {
                        debug!(error = %e, "timeout rejected");
                        break t.min(idle);
                    }
                }
                Ok(Output::Timeout(t)) => break t.min(idle),
                Ok(Output::Transmit(t)) => {
                    self.shared
                        .counters
                        .bytes_out
                        .fetch_add(t.contents.len() as u64, Ordering::Relaxed);
                    if let Some((_, pt)) = session.video
                        && let Some(ts) = timing::frame_end(&t.contents, *pt)
                    {
                        session
                            .timings
                            .out(timing::age(ts, session.epoch.elapsed()));
                    }
                    // Only once media flows, so ICE and DTLS are never part of the simulated outage.
                    if session.connected
                        && let Some(test) = self.config.simulate_loss
                        && test.drops(session.epoch.elapsed())
                    {
                        self.shared
                            .counters
                            .datagrams_dropped_test
                            .fetch_add(1, Ordering::Relaxed);
                    } else if let Some(shaper) = session.shaper.as_mut() {
                        if !shaper.offer(Instant::now(), &t.contents, t.destination) {
                            self.shared
                                .counters
                                .datagrams_dropped_test
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    } else if let Err(e) = self.socket.send_to(&t.contents, t.destination) {
                        debug!(error = %e, to = %t.destination, "send failed");
                    }
                }
                Ok(Output::Event(event)) => {
                    if let Some(why) = handle_event(session, &self.shared, &self.config, event) {
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
            return deadline;
        }
        match self.session.as_mut().and_then(|s| s.shaper.as_mut()) {
            Some(shaper) => {
                let now = Instant::now();
                while let Some((data, to)) = shaper.due(now) {
                    if let Err(e) = self.socket.send_to(&data, to) {
                        debug!(error = %e, %to, "send failed");
                    }
                }
                shaper.next().map_or(deadline, |next| deadline.min(next))
            }
            None => deadline,
        }
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
    config: &Config,
    event: Event,
) -> Option<&'static str> {
    let input = config.input.as_ref();
    match event {
        Event::MediaAdded(added) if added.kind == MediaKind::Video => {
            let pt = session
                .rtc
                .writer(added.mid)
                .and_then(|w| choose_h264(w.payload_params().map(|p| (p.pt(), p.spec()))));
            match pt {
                Some((pt, level_id)) => {
                    let profile_level_id = level_id.map(|v| format!("{v:06x}")).unwrap_or_default();
                    if !config.paced_video
                        && let Some(tx) = session.rtc.direct_api().stream_tx_by_mid(added.mid, None)
                    {
                        tx.set_unpaced(true);
                    }
                    info!(mid = %added.mid, ?pt, %profile_level_id, paced = config.paced_video, "video track ready");
                    session.video = Some((added.mid, pt));
                }
                None => warn!("the browser offered no usable H.264"),
            }
        }
        Event::MediaAdded(added) if added.kind == MediaKind::Audio => {
            let pt = session.rtc.writer(added.mid).and_then(|w| {
                w.payload_params()
                    .find(|p| p.spec().codec == Codec::Opus)
                    .map(|p| p.pt())
            });
            match pt {
                Some(pt) => {
                    info!(mid = %added.mid, ?pt, "audio track ready");
                    session.audio = Some((added.mid, pt));
                }
                None => warn!("the browser offered no Opus"),
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
            if config.adaptive {
                let limits = session.rate.limits();
                session.rtc.bwe().set_bitrate(
                    Bitrate::bps(u64::from(limits.min_bps)),
                    Bitrate::bps(u64::from(limits.max_bps)),
                );
            }
            session.shaper = config
                .simulate_link
                .clone()
                .map(|test| Shaper::new(test, Instant::now()));
            shared.connected.store(true, Ordering::Relaxed);
            // The first frame the browser sees must be an IDR.
            shared.keyframe_wanted.store(true, Ordering::Relaxed);
        }
        Event::EgressBitrateEstimate(BweKind::Twcc {
            estimate,
            can_probe,
        }) if config.adaptive => {
            let before = session.rate.target();
            session.rate.on_estimate(Instant::now(), estimate.as_u64());
            rate_changed(session, shared, before, can_probe);
        }
        Event::MediaEgressStats(stats) if config.adaptive => {
            let before = session.rate.target();
            session
                .rate
                .on_report(Instant::now(), stats.loss, stats.rtt);
            rate_changed(session, shared, before, true);
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
            } else if label == CLIPBOARD_CHANNEL
                && let Some(link) = &config.clipboard
            {
                session.clipboard = Some(id);
                session.clipboard_in = Assembler::default();
                link.enabled.store(true, Ordering::Relaxed);
            } else if label == CONTROL_CHANNEL {
                session.control = Some(id);
                session.shape_sent = false;
                let hello = json!({
                    "t": "hello",
                    "protocol": 1,
                    "codec": "h264",
                    "audio": session.audio.is_some(),
                    "rtp_epoch_ms": session.epoch_unix_ms,
                    "rtp_clock": 90_000,
                });
                send_control(session, &hello);
            }
        }
        Event::ChannelData(data) if Some(data.id) == session.control && !data.binary => {
            if let Some(reply) = control_reply(&data.data, unix_ms(SystemTime::now())) {
                send_control(session, &reply);
            } else if let Some(settings) = parse_settings(&data.data, config.rate) {
                if let Some(max) = settings.max_bps {
                    session.client_max = max;
                }
                if let Some(mode) = settings.mode {
                    session.mode.choose(Instant::now(), mode);
                }
                apply_mode(session, shared, config);
                let max = session.client_max.unwrap_or(config.rate.max_bps);
                info!(
                    max_kbps = max / 1000,
                    mode = session.mode.chosen().name(),
                    adaptive = config.adaptive,
                    "client settings"
                );
                let reply = json!({
                    "t": "settings",
                    "max_bitrate_kbps": max / 1000,
                    "mode": session.mode.chosen().name(),
                    "adaptive": config.adaptive,
                });
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
        Event::ChannelData(data) if Some(data.id) == session.clipboard => {
            receive_clipboard(
                session,
                shared,
                config.clipboard.as_ref(),
                &data.data,
                data.binary,
            );
        }
        Event::ChannelClose(id) => {
            if Some(id) == session.control {
                session.control = None;
            }
            if Some(id) == session.clipboard {
                session.clipboard = None;
                if let Some(link) = &config.clipboard {
                    link.enabled.store(false, Ordering::Relaxed);
                }
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

/// Publishes the rate control's state and logs a new target.
fn rate_changed(session: &Session, shared: &Shared, before: u32, can_probe: bool) {
    shared.rate.publish(&session.rate);
    let target = session.rate.target();
    if target != before {
        shared.counters.rate_changes.fetch_add(1, Ordering::Relaxed);
        let snapshot = shared.rate.snapshot();
        info!(
            target_kbps = target / 1000,
            estimate_kbps = snapshot.estimate_bps.map(|v| v / 1000),
            loss = snapshot.loss,
            rtt_ms = snapshot.rtt.map(|r| r.as_secs_f64() * 1000.0),
            can_probe,
            "rate"
        );
    }
}

/// Labels of the data channels, `docs/PROTOCOL.md`.
const CONTROL_CHANNEL: &str = "control";
const INPUT_FAST_CHANNEL: &str = "input-fast";
const INPUT_RELIABLE_CHANNEL: &str = "input-reliable";
const CLIPBOARD_CHANNEL: &str = "clipboard";

/// One message of the `clipboard` channel: a whole text goes to the clipboard thread. Only sizes are logged.
fn receive_clipboard(
    session: &mut Session,
    shared: &Shared,
    link: Option<&ClipboardLink>,
    data: &[u8],
    binary: bool,
) {
    let counters = &shared.counters;
    let parsed = if binary {
        clipboard::Message::parse(data)
    } else {
        Err(clipboard::ClipboardError::Malformed)
    };
    let text = match parsed {
        Ok(clipboard::Message::Text {
            total,
            offset,
            part,
        }) => session.clipboard_in.add(total, offset, part),
        Ok(_) => Err(clipboard::ClipboardError::Malformed),
        Err(e) => Err(e),
    };
    match (text, link) {
        (Ok(None), _) => {}
        (Ok(Some(text)), Some(link)) => {
            let bytes = text.len();
            match link.incoming.try_send(text) {
                Ok(()) => {
                    counters.clipboard_in.fetch_add(1, Ordering::Relaxed);
                    info!(bytes, "clipboard from the browser");
                }
                Err(_) => {
                    counters.clipboard_rejected.fetch_add(1, Ordering::Relaxed);
                    warn!(bytes, "clipboard queue full, text dropped");
                }
            }
        }
        (Ok(Some(_)), None) => {}
        (Err(e), _) => {
            counters.clipboard_rejected.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "clipboard message refused");
        }
    }
}

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

/// A client `settings` message (D-029, D-030). A field that is not there changes nothing.
#[derive(Debug, Default, PartialEq)]
struct ClientSettings {
    /// `Some(None)`: back to the server's limit.
    max_bps: Option<Option<u32>>,
    mode: Option<StreamMode>,
}

/// Reads `settings`; the limit is kept inside the server's range. A field of the wrong kind spoils the message.
fn parse_settings(text: &[u8], server: RateLimits) -> Option<ClientSettings> {
    let message: Value = serde_json::from_slice(text).ok()?;
    if message["t"].as_str()? != "settings" {
        return None;
    }
    let max_bps = match message.get("max_bitrate_kbps") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let kbps = v.as_f64().filter(|k| k.is_finite())?;
            let bps = (kbps * 1000.0).clamp(f64::from(server.min_bps), f64::from(server.max_bps));
            Some(Some(bps as u32))
        }
    };
    let mode = match message.get("mode") {
        None => None,
        Some(v) => Some(StreamMode::parse(v.as_str()?)?),
    };
    Some(ClientSettings { max_bps, mode })
}

/// The target range for a mode: its top, the server's and the client's limit, whichever is lowest (D-030).
fn session_limits(server: RateLimits, mode: StreamMode, client_max: Option<u32>) -> RateLimits {
    let max_bps = server
        .max_bps
        .min(mode.max_bps())
        .min(client_max.unwrap_or(u32::MAX))
        .max(server.min_bps);
    RateLimits {
        start_bps: server.start_bps.min(max_bps),
        min_bps: server.min_bps,
        max_bps,
    }
}

/// Puts the session's active mode into effect: the target range now, the picture size on the encoder thread.
fn apply_mode(session: &mut Session, shared: &Shared, config: &Config) {
    let active = session.mode.active();
    if config.adaptive {
        let before = session.rate.target();
        session
            .rate
            .set_limits(session_limits(config.rate, active, session.client_max));
        rate_changed(session, shared, before, true);
    }
    let old = shared.mode.swap(active.to_u8(), Ordering::Relaxed);
    if old != active.to_u8() {
        info!(mode = active.name(), "stream mode");
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
    fn settings_fields_are_independent_and_kept_in_range() {
        let server = RateLimits {
            start_bps: 30_000_000,
            min_bps: 2_000_000,
            max_bps: 50_000_000,
        };
        let parse = |text: &str| parse_settings(text.as_bytes(), server);
        let limit = |text: &str| parse(text).and_then(|s| s.max_bps);
        assert_eq!(
            limit(r#"{"t":"settings","max_bitrate_kbps":20000}"#),
            Some(Some(20_000_000))
        );
        assert_eq!(
            limit(r#"{"t":"settings","max_bitrate_kbps":900000}"#),
            Some(Some(50_000_000))
        );
        assert_eq!(
            limit(r#"{"t":"settings","max_bitrate_kbps":10}"#),
            Some(Some(2_000_000))
        );
        assert_eq!(
            limit(r#"{"t":"settings","max_bitrate_kbps":null}"#),
            Some(None)
        );
        assert_eq!(
            parse(r#"{"t":"settings","mode":"weak"}"#),
            Some(ClientSettings {
                max_bps: None,
                mode: Some(StreamMode::Weak)
            })
        );
        assert_eq!(
            parse(r#"{"t":"settings"}"#),
            Some(ClientSettings::default())
        );
        assert_eq!(parse(r#"{"t":"settings","max_bitrate_kbps":"lots"}"#), None);
        assert_eq!(parse(r#"{"t":"settings","mode":"4k"}"#), None);
        assert_eq!(parse(r#"{"t":"ping","id":1}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn the_mode_the_server_and_the_client_cap_the_range() {
        let server = RateLimits {
            start_bps: 30_000_000,
            min_bps: 2_000_000,
            max_bps: 50_000_000,
        };
        let top = |mode, client| session_limits(server, mode, client).max_bps;
        assert_eq!(top(StreamMode::Game, None), 30_000_000);
        assert_eq!(top(StreamMode::Desktop, None), 40_000_000);
        assert_eq!(top(StreamMode::Weak, None), 12_000_000);
        assert_eq!(top(StreamMode::Desktop, Some(20_000_000)), 20_000_000);
        let low = RateLimits {
            max_bps: 25_000_000,
            ..server
        };
        assert_eq!(
            session_limits(low, StreamMode::Desktop, None).max_bps,
            25_000_000
        );
        let weak = session_limits(server, StreamMode::Weak, None);
        assert_eq!((weak.start_bps, weak.min_bps), (12_000_000, 2_000_000));
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
            audio_queue: Mutex::new(VecDeque::new()),
            audio_pool: Mutex::new(Vec::new()),
            clipboard_out: Mutex::new(VecDeque::new()),
            keyframe_wanted: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            mode: AtomicU8::new(0),
            counters: Counters::default(),
            waker: UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap(),
            wake_target: SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
            rate: RateShared::default(),
        }
    }

    #[test]
    fn a_newer_clipboard_text_replaces_one_not_sent_yet() {
        let sink = FrameSink {
            shared: Arc::new(shared()),
        };
        sink.send_clipboard(b"old".to_vec());
        sink.clipboard_applied(3);
        sink.send_clipboard(b"new".to_vec());
        let queue = sink.shared.clipboard_out.lock().unwrap();
        let kinds: Vec<String> = queue.iter().map(|q| format!("{q:?}")).collect();
        assert_eq!(
            kinds,
            vec![
                "Applied(3)".to_string(),
                format!("{:?}", ClipboardOut::Text(b"new".to_vec()))
            ]
        );
    }

    #[test]
    fn a_full_audio_queue_drops_the_oldest_packets_and_reuses_their_buffers() {
        let sink = FrameSink {
            shared: Arc::new(shared()),
        };
        let at = Instant::now();
        for i in 0..AUDIO_QUEUE as u8 + 2 {
            let mut data = sink.audio_buffer();
            data.push(i);
            sink.push_audio(AudioPacket { data, captured: at });
        }
        let queue = sink.shared.audio_queue.lock().unwrap();
        assert_eq!(queue.len(), AUDIO_QUEUE);
        assert_eq!(queue.front().map(|p| p.data[0]), Some(2));
        assert_eq!(
            sink.shared.counters.audio_dropped.load(Ordering::Relaxed),
            2
        );
        drop(queue);
        assert!(sink.audio_buffer().capacity() > 0);
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
            rate: RateLimits {
                start_bps: 30_000_000,
                min_bps: 8_000_000,
                max_bps: 50_000_000,
            },
            adaptive: true,
            paced_video: true,
            simulate_link: None,
            clipboard: None,
        })
        .expect("bind on loopback");
        assert!(server.open("not an sdp".into()).is_err());
        let sink = server.sink();
        assert!(!sink.connected());
        sink.push(frame(1));
        drop(server);
    }
}
