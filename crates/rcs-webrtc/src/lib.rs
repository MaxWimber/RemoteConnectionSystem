//! WebRTC transport on `str0m` (D-020): ICE-lite on one UDP port, one session at a time, H.264 to the browser.
//!
//! All network work runs on one thread that owns the socket and the `Rtc`. The encoder hands frames over through
//! [`FrameSink`]: a queue of two frames. When it overflows, the oldest frame is dropped and an IDR is requested, so the
//! capture thread never waits for the network. A one-byte datagram from a loopback socket wakes the network thread
//! when a frame or a command arrives.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use str0m::change::SdpOffer;
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
}

struct Shared {
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
    epoch: Instant,
}

struct NetworkLoop {
    socket: UdpSocket,
    config: Config,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    session: Option<Session>,
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
        self.shared
            .counters
            .sessions
            .fetch_add(1, Ordering::Relaxed);
        self.session = Some(Session {
            rtc,
            video: None,
            connected: false,
            epoch: now,
        });
        info!("session opened");
        Ok(answer.to_sdp_string())
    }

    fn end_session(&mut self, why: &str) {
        if let Some(mut s) = self.session.take() {
            s.rtc.disconnect();
            info!(why, "session ended");
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
                    if let Err(e) = self.socket.send_to(&t.contents, t.destination) {
                        debug!(error = %e, to = %t.destination, "send failed");
                    }
                }
                Ok(Output::Event(event)) => {
                    if let Some(why) = handle_event(session, &self.shared, event) {
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
fn handle_event(session: &mut Session, shared: &Shared, event: Event) -> Option<&'static str> {
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
        Event::ChannelOpen(_, label) => info!(%label, "data channel open"),
        Event::ChannelClose(_) => debug!("data channel closed"),
        _ => {}
    }
    None
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
        })
        .expect("bind on loopback");
        assert!(server.open("not an sdp".into()).is_err());
        let sink = server.sink();
        assert!(!sink.connected());
        sink.push(frame(1));
        drop(server);
    }
}
