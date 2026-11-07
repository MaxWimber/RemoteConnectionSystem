//! Screen capture, encoding, WebRTC and, from phase 4, input.
//!
//! Phase 2: the agent runs as the current user, sends the desktop to one browser over WebRTC and answers `rcs-web`
//! on `\\.\pipe\rcs-agent`. UDP listens on loopback unless `--udp` names another address.

mod cursor;
mod input;
mod pipe;
mod pipeline;
mod recovery;
mod resources;

use std::fs::{self, OpenOptions};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

use rcs_encode::Backend;
use rcs_protocol::BUILD_ID;
use rcs_protocol::agent::{AgentStatus, PIPE_NAME, Request, Response};
use tracing::{Level, error, info, info_span};

const USAGE: &str = "\
Usage: rcs-agent [--udp ADDR] [--advertise ADDR,ADDR] [--pipe NAME] [--log DIR]
                 [--encoder auto|amf|mf] [--width W] [--height H] [--fps F] [--bitrate-mbps B] [--display N]
                 [--min-bitrate-mbps B] [--max-bitrate-mbps B] [--rate adaptive|fixed]
                 [--input record|sendinput] [--stop-event NAME]
                 [--simulate-loss EVERY_MS:BURST_MS]   testing only: stop sending for BURST_MS every EVERY_MS
                 [--simulate-link S:MBPS,S:MBPS] [--simulate-link-queue-ms MS]
                 [--simulate-amf-failure FRAMES]       testing only: every AMF encoder fails after FRAMES frames
                                                       testing only: a narrow link on the way out, rates repeat
                 [--simulate-amf-failure FRAMES]       testing only: every AMF encoder fails after FRAMES frames
  --rate       adaptive (default) follows the network between the minimum and maximum bitrate (D-027); fixed
               keeps --bitrate-mbps
  --udp        UDP address for WebRTC, default 127.0.0.1:8443
  --input      record (default) checks and counts input without sending it to Windows; sendinput controls this
               computer (D-022)
  --stop-event the supervisor's event: when it is set the agent lets go of all input and exits (D-025)
  --advertise  addresses written into the answer, default the --udp address, or the LAN address and loopback
               when --udp is 0.0.0.0";

struct Options {
    udp: SocketAddr,
    advertise: Vec<SocketAddr>,
    pipe: String,
    log: PathBuf,
    pipeline: pipeline::Config,
    simulate_loss: Option<rcs_webrtc::LossTest>,
    simulate_link: Option<String>,
    simulate_link_queue_ms: u64,
    rate: rcs_webrtc::RateLimits,
    adaptive: bool,
    input: input::Mode,
    stop_event: Option<String>,
}

/// Input messages waiting for the input thread (D-022).
const INPUT_QUEUE: usize = 1024;
/// How often the agent logs what it holds (P8-7).
const RESOURCES_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// New pointer shapes waiting for the input thread; a shape changes a few times a second at most.
const SHAPE_QUEUE: usize = 4;

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        udp: SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)),
        advertise: Vec::new(),
        pipe: PIPE_NAME.into(),
        log: PathBuf::from("out").join("agent"),
        simulate_loss: None,
        simulate_link: None,
        simulate_link_queue_ms: 200,
        rate: rcs_webrtc::RateLimits {
            start_bps: 30_000_000,
            // Under 8 Mbit/s 1080p looks poor, but a weak link must still carry a picture (D-027).
            min_bps: 2_000_000,
            max_bps: 50_000_000,
        },
        adaptive: true,
        input: input::Mode::Record,
        stop_event: None,
        pipeline: pipeline::Config {
            display: 0,
            backend: Backend::Auto,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 30_000_000,
            simulate_amf_failure: None,
        },
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<i64>()
                .ok()
                .filter(|v| (1..=100_000).contains(v))
                .ok_or_else(|| format!("{flag}: bad number {value}"))
        };
        match flag.as_str() {
            "--udp" => options.udp = value.parse().map_err(|e| format!("{flag}: {e}"))?,
            "--advertise" => {
                options.advertise = value
                    .split(',')
                    .map(|a| a.trim().parse().map_err(|e| format!("{flag}: {a}: {e}")))
                    .collect::<Result<_, _>>()?
            }
            "--pipe" => options.pipe = value.clone(),
            "--stop-event" => options.stop_event = Some(value.clone()),
            "--input" => {
                options.input = input::Mode::parse(value)
                    .ok_or_else(|| format!("{flag}: record or sendinput"))?
            }
            "--log" => options.log = PathBuf::from(value),
            "--encoder" => {
                options.pipeline.backend =
                    Backend::parse(value).ok_or_else(|| format!("{flag}: auto, amf or mf"))?
            }
            "--width" => options.pipeline.width = number()? as i32,
            "--height" => options.pipeline.height = number()? as i32,
            "--fps" => options.pipeline.fps = number()? as i32,
            "--bitrate-mbps" => options.pipeline.bitrate = number()? * 1_000_000,
            "--min-bitrate-mbps" => options.rate.min_bps = (number()? * 1_000_000) as u32,
            "--max-bitrate-mbps" => options.rate.max_bps = (number()? * 1_000_000) as u32,
            "--rate" => {
                options.adaptive = match value.as_str() {
                    "adaptive" => true,
                    "fixed" => false,
                    _ => return Err(format!("{flag}: adaptive or fixed")),
                }
            }
            "--simulate-link" => options.simulate_link = Some(value.clone()),
            "--simulate-link-queue-ms" => options.simulate_link_queue_ms = number()? as u64,
            "--simulate-amf-failure" => {
                options.pipeline.simulate_amf_failure = Some(number()? as u64)
            }
            "--display" => options.pipeline.display = number().unwrap_or(0) as u32,
            "--simulate-loss" => {
                let (every, burst) = value
                    .split_once(':')
                    .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                    .filter(|(a, b)| *b > 0 && b < a)
                    .ok_or_else(|| {
                        format!("{flag}: EVERY_MS:BURST_MS, burst shorter than every")
                    })?;
                options.simulate_loss = Some(rcs_webrtc::LossTest {
                    every: std::time::Duration::from_millis(every),
                    burst: std::time::Duration::from_millis(burst),
                });
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if options.advertise.is_empty() {
        options.advertise = default_advertise(options.udp);
    }
    options.rate.start_bps = options.pipeline.bitrate.min(i64::from(u32::MAX)) as u32;
    if options.rate.min_bps > options.rate.max_bps {
        return Err("--min-bitrate-mbps is above --max-bitrate-mbps".into());
    }
    if !options.adaptive {
        options.rate.min_bps = options.rate.start_bps;
        options.rate.max_bps = options.rate.start_bps;
    }
    Ok(options)
}

/// The LAN address of the default route, found without sending anything.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn default_advertise(udp: SocketAddr) -> Vec<SocketAddr> {
    if !udp.ip().is_unspecified() {
        return vec![udp];
    }
    let mut list: Vec<SocketAddr> = lan_address()
        .map(|ip| SocketAddr::new(ip, udp.port()))
        .into_iter()
        .collect();
    list.push(SocketAddr::from((Ipv4Addr::LOCALHOST, udp.port())));
    list
}

fn init_logging(dir: &PathBuf) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("rcs-agent.jsonl");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_max_level(Level::INFO)
        .with_writer(Mutex::new(file))
        .try_init()
        .map_err(|e| format!("cannot start logging: {e}"))?;
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, %backtrace, "panic");
        eprintln!("{info}");
    }));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = init_logging(&options.log) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "agent stopped");
            eprintln!("Ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: Options) -> Result<(), String> {
    let _span = info_span!("rcs", build = BUILD_ID, tool = "agent").entered();
    let _ = rcs_capture::dxgi::set_dpi_awareness();
    let (input_sender, input_receiver) = sync_channel(INPUT_QUEUE);
    let link = rcs_webrtc::InputLink {
        sender: input_sender,
        release: Arc::new(AtomicBool::new(false)),
        live: Arc::new(AtomicU64::new(0)),
    };
    let simulate_link = options
        .simulate_link
        .as_deref()
        .map(|steps| rcs_webrtc::LinkTest::parse(steps, options.simulate_link_queue_ms))
        .transpose()
        .map_err(|e| format!("--simulate-link: {e}"))?;
    let server = rcs_webrtc::Server::start(rcs_webrtc::Config {
        bind: options.udp,
        advertise: options.advertise.clone(),
        simulate_loss: options.simulate_loss,
        input: Some(link.clone()),
        rate: options.rate,
        adaptive: options.adaptive,
        simulate_link,
    })
    .map_err(|e| format!("UDP {}: {e}", options.udp))?;
    info!(udp = %server.local_addr(), advertise = ?options.advertise, pipe = %options.pipe,
        input = options.input.name(), adaptive = options.adaptive,
        min_kbps = options.rate.min_bps / 1000, max_kbps = options.rate.max_bps / 1000, "agent started");
    println!(
        "rcs-agent {BUILD_ID}: WebRTC {} ({:?}), канал {}, ввод {}",
        server.local_addr(),
        options.advertise,
        options.pipe,
        options.input.name()
    );

    let stats = Arc::new(pipeline::Stats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let input_stats = Arc::new(input::Stats::default());
    let (shape_sender, shape_receiver) = sync_channel(SHAPE_QUEUE);
    let monitor_stats = stats.clone();
    let input_thread = input::Thread {
        mode: options.input,
        receiver: input_receiver,
        release: link.release.clone(),
        live: link.live.clone(),
        monitor: Arc::new(move || monitor_stats.info.lock().ok().and_then(|i| i.monitor)),
        stats: input_stats.clone(),
        stop: stop.clone(),
        cursor: Some((shape_receiver, server.sink())),
    }
    .spawn()
    .map_err(|e| format!("input thread: {e}"))?;
    let input_thread = Arc::new(Mutex::new(Some(input_thread)));
    if let Some(name) = &options.stop_event {
        watch_stop_event(name, stop.clone(), input_thread.clone())?;
    }
    let worker = pipeline::spawn(
        options.pipeline.clone(),
        server.sink(),
        shape_sender,
        stats.clone(),
        stop.clone(),
    )
    .map_err(|e| format!("pipeline thread: {e}"))?;
    // Memory, handles and video memory every 10 s, for long runs (P8-7).
    let held = Arc::new(Mutex::new(resources::Sample::default()));
    let resources_thread = resources::spawn(RESOURCES_EVERY, held.clone(), stop.clone())
        .map_err(|e| format!("resources thread: {e}"))?;

    let config = options.pipeline.clone();
    let served = pipe::serve(&options.pipe, |request| match request {
        Request::OpenSession {
            session_id,
            sdp_offer,
        } => match server.open(sdp_offer) {
            Ok(sdp_answer) => {
                info!(%session_id, "session opened");
                Response::SessionOpened {
                    session_id,
                    sdp_answer,
                }
            }
            Err(message) => Response::Error {
                code: "offer_rejected".into(),
                message,
            },
        },
        Request::CloseSession { session_id } => {
            server.close();
            Response::SessionClosed { session_id }
        }
        Request::Status => Response::AgentStatus(Box::new(status(
            &server,
            &stats,
            &config,
            options.input,
            &input_stats,
            &held,
        ))),
    });
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    let _ = resources_thread.join();
    if let Some(t) = input_thread.lock().ok().and_then(|mut t| t.take()) {
        let _ = t.join();
    }
    served
}

/// Waits for the supervisor's stop event on its own thread. Then the input thread lets go of every key, button and
/// contact and unplugs the gamepads, and the process exits: the pipe server blocks in `ConnectNamedPipe` and has
/// nothing to finish.
fn watch_stop_event(
    name: &str,
    stop: Arc<AtomicBool>,
    input_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) -> Result<(), String> {
    use windows::Win32::System::Threading::{
        INFINITE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: the name outlives the call; the handle is kept by the watcher thread until the process exits.
    let event = unsafe {
        OpenEventW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            &windows::core::HSTRING::from(name),
        )
    }
    .map_err(|e| format!("stop event {name}: {e}"))?;
    let raw = event.0 as usize;
    std::thread::Builder::new()
        .name("stop-event".into())
        .spawn(move || {
            let event = windows::Win32::Foundation::HANDLE(raw as *mut _);
            // SAFETY: the handle stays open for the life of the process.
            unsafe { WaitForSingleObject(event, INFINITE) };
            info!("stop requested by the supervisor");
            stop.store(true, Ordering::Relaxed);
            if let Some(t) = input_thread.lock().ok().and_then(|mut t| t.take()) {
                let _ = t.join();
            }
            info!("agent stopped");
            std::process::exit(0);
        })
        .map_err(|e| format!("stop thread: {e}"))?;
    Ok(())
}

fn status(
    server: &rcs_webrtc::Server,
    stats: &pipeline::Stats,
    config: &pipeline::Config,
    mode: input::Mode,
    input: &input::Stats,
    held: &Mutex<resources::Sample>,
) -> AgentStatus {
    let held = held.lock().map(|h| *h).unwrap_or_default();
    let c = server.counters();
    let rate = server.rate();
    let info = stats.info.lock().map(|i| i.clone()).unwrap_or_default();
    let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
    let input_error = input.last_error.lock().ok().and_then(|e| e.clone());
    let gamepad = input.gamepad.lock().map(|g| g.clone()).unwrap_or_default();
    AgentStatus {
        build: BUILD_ID.into(),
        connected: server.sink().connected(),
        encoder: info.encoder,
        encoder_open: info.open,
        adapter: info.adapter,
        width: config.width as u32,
        height: config.height as u32,
        fps: config.fps as u32,
        bitrate: config.bitrate as u64,
        sessions: c.sessions,
        frames_encoded: stats.frames_encoded.load(Ordering::Relaxed),
        frames_sent: c.frames_sent,
        bytes_sent: c.bytes_sent,
        frames_dropped_queue: c.frames_dropped_queue,
        keyframe_requests: c.keyframe_requests,
        datagrams_dropped_test: c.datagrams_dropped_test,
        pipeline_restarts: stats.restarts.load(Ordering::Relaxed),
        last_error: info.last_error.or(input_error),
        input: mode.name().into(),
        input_events: load(&input.events),
        input_rejected: c.input_rejected,
        input_dropped: c.input_dropped + load(&input.stale),
        input_errors: load(&input.errors),
        input_releases: load(&input.releases),
        gamepad,
        gamepad_events: load(&input.gamepad_events),
        bitrate_target: u64::from(rate.target_bps),
        bitrate_estimate: rate.estimate_bps.map(u64::from),
        loss: rate.loss,
        rtt_ms: rate.rtt.map(|r| r.as_secs_f64() * 1000.0),
        rate_changes: c.rate_changes,
        private_bytes: held.private_bytes,
        handles: held.handles,
        gpu_memory_bytes: held.gpu_local,
    }
}
