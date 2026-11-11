//! Experimental video over WebTransport (P9-4, D-043): every encoded frame in its own unidirectional QUIC stream.
//!
//! The server listens on loopback only, with a self-signed certificate the browser accepts by its hash. A browser
//! gets in with a one-time token that the agent hands out inside the signed-in WebRTC session. All QUIC work runs
//! on its own thread with a single-threaded tokio runtime; the agent hands frames over without waiting.
//!
//! A frame message: `u64 capture time` in Unix microseconds, `u32 seq`, `u8 flags` (bit 0 keyframe), then H.264
//! Annex B, little-endian. A gap in `seq` tells the browser to wait for the next IDR.
//! While more than `MAX_IN_FLIGHT` frames are unacknowledged, new P-frames are dropped and an IDR is asked for;
//! a frame not acknowledged within `STALE` is reset. The bitrate follows the same `RateControl` as WebRTC, fed with
//! the QUIC congestion window over RTT instead of the `str0m` estimate.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rcs_webrtc::{RateControl, RateLimits};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wtransport::{Endpoint, Identity, ServerConfig, VarInt};

/// Frames in flight before new P-frames are dropped.
const MAX_IN_FLIGHT: u64 = 3;
/// A frame still unacknowledged after this is of no use any more: its stream is reset.
const STALE: Duration = Duration::from_millis(300);
const QUEUE: usize = 4;
const RATE_EVERY: Duration = Duration::from_millis(100);
const PATH: &str = "/rcs-video";

/// One encoded frame for the browser.
#[derive(Debug)]
pub struct WtFrame {
    pub data: Vec<u8>,
    pub captured_unix_us: u64,
    pub keyframe: bool,
}

/// What the WebTransport side asks of the encoder.
#[derive(Clone)]
pub struct Hooks {
    /// An IDR is needed: a frame was dropped or a browser connected.
    pub keyframe: Arc<dyn Fn() + Send + Sync>,
    /// The bitrate the encoder should use now.
    pub target: Arc<dyn Fn(u32) + Send + Sync>,
}

/// Counters for the agent's log.
#[derive(Debug, Default)]
pub struct WtCounters {
    pub sessions: AtomicU64,
    pub refused: AtomicU64,
    pub frames_sent: AtomicU64,
    pub frames_acked: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub frames_reset: AtomicU64,
    pub bytes_sent: AtomicU64,
}

pub struct WtServer {
    url: String,
    hash: String,
    token: Arc<Mutex<Option<String>>>,
    frames: mpsc::Sender<WtFrame>,
    connected: Arc<AtomicBool>,
    pub counters: Arc<WtCounters>,
    _thread: JoinHandle<()>,
}

/// 128 random bits from the system generator, hex.
fn random_token() -> Option<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl WtServer {
    pub fn start(bind: SocketAddr, limits: RateLimits, hooks: Hooks) -> Result<Self, String> {
        if !bind.ip().is_loopback() {
            return Err("WebTransport listens on loopback only (D-043)".into());
        }
        let identity = Identity::self_signed_builder()
            .subject_alt_names(["localhost", "127.0.0.1"])
            .from_now_utc()
            .validity_days(13)
            .build()
            .map_err(|e| format!("certificate: {e}"))?;
        let digest = identity.certificate_chain().as_slice()[0].hash();
        let hash: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let config = ServerConfig::builder()
            .with_bind_address(bind)
            .with_identity(identity)
            .keep_alive_interval(Some(Duration::from_secs(3)))
            .build();
        let (frames, receiver) = mpsc::channel(QUEUE);
        let token = Arc::new(Mutex::new(None));
        let connected = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(WtCounters::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_token = token.clone();
        let thread_connected = connected.clone();
        let thread_counters = counters.clone();
        let thread = std::thread::Builder::new()
            .name("webtransport".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio: {e}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let endpoint = match Endpoint::server(config) {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("bind {bind}: {e}")));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    serve(
                        endpoint,
                        receiver,
                        thread_token,
                        thread_connected,
                        thread_counters,
                        limits,
                        hooks,
                    )
                    .await;
                });
            })
            .map_err(|e| format!("thread: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "WebTransport thread ended".to_string())??;
        info!(%bind, "webtransport server started");
        Ok(Self {
            url: format!("https://127.0.0.1:{}{PATH}", bind.port()),
            hash,
            token,
            frames,
            connected,
            counters,
            _thread: thread,
        })
    }

    /// The address, the certificate hash and a fresh one-time token for one browser, as the JSON of `hello`.
    pub fn offer(&self, url_port: Option<u16>) -> Option<(String, String, String)> {
        let token = random_token()?;
        if let Ok(mut slot) = self.token.lock() {
            *slot = Some(token.clone());
        }
        let url = match url_port {
            Some(port) => format!("https://127.0.0.1:{port}{PATH}"),
            None => self.url.clone(),
        };
        Some((url, self.hash.clone(), token))
    }

    /// Whether a browser takes video over WebTransport now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Hands a frame over without waiting; false when the queue was full and it was dropped.
    pub fn push(&self, frame: WtFrame) -> bool {
        self.frames.try_send(frame).is_ok()
    }
}

async fn serve(
    endpoint: Endpoint<wtransport::endpoint::endpoint_side::Server>,
    mut frames: mpsc::Receiver<WtFrame>,
    token: Arc<Mutex<Option<String>>>,
    connected: Arc<AtomicBool>,
    counters: Arc<WtCounters>,
    limits: RateLimits,
    hooks: Hooks,
) {
    let mut session: Option<wtransport::Connection> = None;
    let mut rate = RateControl::new(limits);
    let mut in_flight = Arc::new(AtomicU64::new(0));
    let mut wants_keyframe = false;
    let mut last_stats = (Instant::now(), 0u64, 0u64);
    let mut rate_tick = tokio::time::interval(RATE_EVERY);
    let mut priority = 0i32;
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Ok(request) = incoming.await else { continue };
                let expected = token.lock().ok().and_then(|mut t| t.take());
                let given = request.path().split_once("?token=").map(|(_, t)| t.to_string());
                if request.path().split('?').next() != Some(PATH) || expected.is_none() || given != expected {
                    counters.refused.fetch_add(1, Ordering::Relaxed);
                    warn!("webtransport session refused: no valid token");
                    request.forbidden().await;
                    continue;
                }
                match request.accept().await {
                    Ok(c) => {
                        counters.sessions.fetch_add(1, Ordering::Relaxed);
                        info!("webtransport session accepted");
                        session = Some(c);
                        in_flight = Arc::new(AtomicU64::new(0));
                        rate = RateControl::new(limits);
                        (hooks.target)(rate.target());
                        connected.store(true, Ordering::Relaxed);
                        wants_keyframe = true;
                        (hooks.keyframe)();
                    }
                    Err(e) => warn!(error = %e, "webtransport accept failed"),
                }
            }
            frame = frames.recv() => {
                let Some(frame) = frame else { return };
                let Some(conn) = session.as_ref() else { continue };
                if let Some(reason) = conn.quic_connection().close_reason() {
                    debug!(?reason, "webtransport session closed");
                    session = None;
                    connected.store(false, Ordering::Relaxed);
                    (hooks.target)(0);
                    continue;
                }
                let busy = in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT;
                if !frame.keyframe && (busy || wants_keyframe) {
                    counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                    if !wants_keyframe {
                        wants_keyframe = true;
                        (hooks.keyframe)();
                    }
                    continue;
                }
                wants_keyframe = false;
                priority = priority.wrapping_add(1);
                rate.on_sent(Instant::now(), frame.data.len());
                send_frame(conn.clone(), frame, priority, in_flight.clone(), counters.clone(), hooks.clone());
            }
            _ = rate_tick.tick() => {
                let Some(conn) = session.as_ref() else { continue };
                if conn.quic_connection().close_reason().is_some() {
                    // The browser left: give the bitrate back to WebRTC at once.
                    info!("webtransport session closed");
                    session = None;
                    connected.store(false, Ordering::Relaxed);
                    (hooks.target)(0);
                    continue;
                }
                let now = Instant::now();
                let stats = conn.quic_connection().stats();
                let rtt = stats.path.rtt;
                if rtt > Duration::ZERO {
                    let estimate = stats.path.cwnd as f64 * 8.0 / rtt.as_secs_f64();
                    rate.on_estimate(now, estimate as u64);
                }
                if now.duration_since(last_stats.0) >= Duration::from_secs(1) {
                    let (sent, lost) = (stats.path.sent_packets, stats.path.lost_packets);
                    let ds = sent.saturating_sub(last_stats.1);
                    let dl = lost.saturating_sub(last_stats.2);
                    let loss = (ds > 0).then(|| dl as f32 / ds as f32);
                    rate.on_report(now, loss, Some(rtt));
                    last_stats = (now, sent, lost);
                    info!(
                        target_kbps = rate.target() / 1000,
                        estimate_kbps = rate.estimate().map(|v| v / 1000),
                        cwnd = stats.path.cwnd,
                        rtt_ms = rtt.as_secs_f64() * 1000.0,
                        loss,
                        in_flight = in_flight.load(Ordering::Relaxed),
                        sent = counters.frames_sent.load(Ordering::Relaxed),
                        acked = counters.frames_acked.load(Ordering::Relaxed),
                        dropped = counters.frames_dropped.load(Ordering::Relaxed),
                        reset = counters.frames_reset.load(Ordering::Relaxed),
                        "webtransport sample"
                    );
                }
                rate.on_tick(now);
                (hooks.target)(rate.target());
            }
        }
    }
}

/// One frame in its own stream; counted in flight until the browser acknowledged all of it, reset when stale.
fn send_frame(
    conn: wtransport::Connection,
    frame: WtFrame,
    priority: i32,
    in_flight: Arc<AtomicU64>,
    counters: Arc<WtCounters>,
    hooks: Hooks,
) {
    in_flight.fetch_add(1, Ordering::Relaxed);
    counters.frames_sent.fetch_add(1, Ordering::Relaxed);
    counters
        .bytes_sent
        .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    tokio::spawn(async move {
        let mut header = [0u8; 13];
        header[..8].copy_from_slice(&frame.captured_unix_us.to_le_bytes());
        header[8..12].copy_from_slice(&(priority as u32).to_le_bytes());
        header[12] = u8::from(frame.keyframe);
        let opened = async {
            let opening = conn.open_uni().await.map_err(|e| e.to_string())?;
            opening.await.map_err(|e| e.to_string())
        };
        let mut stream = match opened.await {
            Ok(s) => s,
            Err(e) => {
                debug!(error = %e, "webtransport stream not opened");
                in_flight.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        };
        stream.set_priority(priority);
        let result = tokio::time::timeout(STALE, async {
            stream.write_all(&header).await?;
            stream.write_all(&frame.data).await?;
            stream.finish().await
        })
        .await;
        match result {
            Ok(Ok(())) => {
                counters.frames_acked.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(e)) => debug!(error = %e, "webtransport frame failed"),
            Err(_) => {
                // Too late to show: stop retransmitting it and start again from an IDR.
                let _ = stream.reset(VarInt::from_u32(1));
                counters.frames_reset.fetch_add(1, Ordering::Relaxed);
                (hooks.keyframe)();
            }
        }
        in_flight.fetch_sub(1, Ordering::Relaxed);
    });
}
