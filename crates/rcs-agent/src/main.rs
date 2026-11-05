//! Screen capture, encoding, WebRTC and, from phase 4, input.
//!
//! Phase 2: the agent runs as the current user, sends the desktop to one browser over WebRTC and answers `rcs-web`
//! on `\\.\pipe\rcs-agent`. UDP listens on loopback unless `--udp` names another address.

mod pipe;
mod pipeline;

use std::fs::{self, OpenOptions};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rcs_encode::Backend;
use rcs_protocol::BUILD_ID;
use rcs_protocol::agent::{AgentStatus, PIPE_NAME, Request, Response};
use tracing::{Level, error, info, info_span};

const USAGE: &str = "\
Usage: rcs-agent [--udp ADDR] [--advertise ADDR,ADDR] [--pipe NAME] [--log DIR]
                 [--encoder auto|amf|mf] [--width W] [--height H] [--fps F] [--bitrate-mbps B] [--display N]
                 [--simulate-loss EVERY_MS:BURST_MS]   testing only: stop sending for BURST_MS every EVERY_MS
  --udp        UDP address for WebRTC, default 127.0.0.1:8443
  --advertise  addresses written into the answer, default the --udp address, or the LAN address and loopback
               when --udp is 0.0.0.0";

struct Options {
    udp: SocketAddr,
    advertise: Vec<SocketAddr>,
    pipe: String,
    log: PathBuf,
    pipeline: pipeline::Config,
    simulate_loss: Option<rcs_webrtc::LossTest>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        udp: SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)),
        advertise: Vec::new(),
        pipe: PIPE_NAME.into(),
        log: PathBuf::from("out").join("agent"),
        simulate_loss: None,
        pipeline: pipeline::Config {
            display: 0,
            backend: Backend::Auto,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 30_000_000,
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
            "--log" => options.log = PathBuf::from(value),
            "--encoder" => {
                options.pipeline.backend =
                    Backend::parse(value).ok_or_else(|| format!("{flag}: auto, amf or mf"))?
            }
            "--width" => options.pipeline.width = number()? as i32,
            "--height" => options.pipeline.height = number()? as i32,
            "--fps" => options.pipeline.fps = number()? as i32,
            "--bitrate-mbps" => options.pipeline.bitrate = number()? * 1_000_000,
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
    let server = rcs_webrtc::Server::start(rcs_webrtc::Config {
        bind: options.udp,
        advertise: options.advertise.clone(),
        simulate_loss: options.simulate_loss,
    })
    .map_err(|e| format!("UDP {}: {e}", options.udp))?;
    info!(udp = %server.local_addr(), advertise = ?options.advertise, pipe = %options.pipe, "agent started");
    println!(
        "rcs-agent {BUILD_ID}: WebRTC {} ({:?}), канал {}",
        server.local_addr(),
        options.advertise,
        options.pipe
    );

    let stats = Arc::new(pipeline::Stats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let worker = pipeline::spawn(
        options.pipeline.clone(),
        server.sink(),
        stats.clone(),
        stop.clone(),
    )
    .map_err(|e| format!("pipeline thread: {e}"))?;

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
        Request::Status => Response::AgentStatus(status(&server, &stats, &config)),
    });
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    served
}

fn status(
    server: &rcs_webrtc::Server,
    stats: &pipeline::Stats,
    config: &pipeline::Config,
) -> AgentStatus {
    let c = server.counters();
    let info = stats.info.lock().map(|i| i.clone()).unwrap_or_default();
    AgentStatus {
        build: BUILD_ID.into(),
        connected: server.sink().connected(),
        encoder: info.encoder,
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
        last_error: info.last_error,
    }
}
