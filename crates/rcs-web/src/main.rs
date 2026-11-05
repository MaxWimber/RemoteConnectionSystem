//! `rcs-web`: the web client, signaling and, from phase 3, sign-in and files.
//!
//! Phase 2 has no sign-in yet, so the server listens on loopback by default and Caddy does not proxy it
//! (`docs/ROADMAP.md`). Signaling follows `docs/PROTOCOL.md`; the SDP goes to the agent over its pipe.

mod agent;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use rcs_protocol::agent::{PIPE_NAME, Request, Response as AgentResponse};
use rcs_protocol::{BUILD_ID, PROTOCOL_VERSION};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{info, warn};

const USAGE: &str = "\
Usage: rcs-web [--listen ADDR] [--static DIR] [--pipe NAME] [--base PATH]
  --listen  default 127.0.0.1:8080
  --static  the built web client, default web/dist
  --base    URL prefix, default /RemoteConnectionSystem";

struct Options {
    listen: SocketAddr,
    static_dir: PathBuf,
    pipe: String,
    base: String,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
        static_dir: PathBuf::from("web").join("dist"),
        pipe: PIPE_NAME.into(),
        base: "/RemoteConnectionSystem".into(),
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => options.listen = value.parse().map_err(|e| format!("{flag}: {e}"))?,
            "--static" => options.static_dir = PathBuf::from(value),
            "--pipe" => options.pipe = value.clone(),
            "--base" => options.base = value.trim_end_matches('/').to_string(),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if !options.base.starts_with('/') {
        return Err("--base must start with /".into());
    }
    Ok(options)
}

/// The browser that owns the media session, so a new one can tell the old one it was replaced.
struct Owner {
    session_id: String,
    notify: mpsc::Sender<Value>,
}

struct AppState {
    agent: agent::Client,
    owner: Mutex<Option<Owner>>,
    next_id: AtomicU64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .init();
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: Options) -> Result<(), String> {
    let state = Arc::new(AppState {
        agent: agent::Client::new(options.pipe.clone()),
        owner: Mutex::new(None),
        next_id: AtomicU64::new(1),
    });
    let index = options.static_dir.join("index.html");
    let client = ServeDir::new(&options.static_dir).fallback(ServeFile::new(&index));
    let api = Router::new()
        .route("/api/signal", get(signal))
        .route("/api/status", get(status))
        .route("/api/health", get(|| async { "ok" }))
        .with_state(state);
    let base = options.base.clone();
    // A nested router does not match its own path with a trailing slash, which is the client's address.
    let app = Router::new()
        .route_service(&format!("{base}/"), ServeFile::new(&index))
        .nest(&base, api.fallback_service(client))
        .route(
            "/",
            get(move || async move { Redirect::temporary(&format!("{base}/")) }),
        )
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        // The browser asks again every time, so a new client build is never hidden behind a cached index.html;
        // unchanged files come back as 304 thanks to Last-Modified.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));
    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .map_err(|e| format!("listen {}: {e}", options.listen))?;
    info!(listen = %options.listen, base = %options.base, "rcs-web started");
    println!(
        "rcs-web {BUILD_ID}: http://{}{}/",
        options.listen, options.base
    );
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("server: {e}"))
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    match state.agent.call(Request::Status).await {
        Ok(AgentResponse::AgentStatus(s)) => axum::Json(s).into_response(),
        Ok(other) => (StatusCode::BAD_GATEWAY, format!("{other:?}")).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    }
}

async fn signal(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| serve_socket(socket, state))
}

fn message(kind: &str) -> Value {
    json!({ "v": PROTOCOL_VERSION, "type": kind })
}

fn error_message(code: &str) -> Value {
    json!({ "v": PROTOCOL_VERSION, "type": "error", "code": code })
}

async fn serve_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let (notify, mut notices) = mpsc::channel::<Value>(4);
    let mut session: Option<String> = None;
    info!(socket = id, "signaling connected");
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(frame)) = incoming else { break };
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let reply = handle(&state, id, &text, &notify, &mut session).await;
                if let Some(reply) = reply
                    && socket.send(Message::Text(reply.to_string().into())).await.is_err()
                {
                    break;
                }
            }
            Some(notice) = notices.recv() => {
                // Another browser took the session; this one keeps the socket but has no media.
                session = None;
                if socket.send(Message::Text(notice.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    // A browser that leaves ends its session, unless another browser already replaced it.
    if let Some(session_id) = session {
        let mut owner = state.owner.lock().await;
        if owner.as_ref().is_some_and(|o| o.session_id == session_id) {
            *owner = None;
            drop(owner);
            let _ = state.agent.call(Request::CloseSession { session_id }).await;
        }
    }
    info!(socket = id, "signaling closed");
}

async fn handle(
    state: &AppState,
    socket_id: u64,
    text: &str,
    notify: &mpsc::Sender<Value>,
    session: &mut Option<String>,
) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return Some(error_message("bad_message"));
    };
    if msg["v"].as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
        return Some(error_message("protocol_mismatch"));
    }
    match msg["type"].as_str() {
        Some("ping") => Some(json!({ "v": PROTOCOL_VERSION, "type": "pong", "id": msg["id"] })),
        Some("offer") => {
            let Some(sdp) = msg["sdp"].as_str() else {
                return Some(error_message("bad_message"));
            };
            let session_id = format!(
                "{socket_id}-{}",
                state.next_id.fetch_add(1, Ordering::Relaxed)
            );
            let reply = state
                .agent
                .call(Request::OpenSession {
                    session_id: session_id.clone(),
                    sdp_offer: sdp.to_string(),
                })
                .await;
            match reply {
                Ok(AgentResponse::SessionOpened { sdp_answer, .. }) => {
                    let previous = state.owner.lock().await.replace(Owner {
                        session_id: session_id.clone(),
                        notify: notify.clone(),
                    });
                    if let Some(previous) = previous
                        && previous.session_id != session_id
                        && !previous.notify.same_channel(notify)
                    {
                        let _ = previous.notify.try_send(message("session-replaced"));
                    }
                    *session = Some(session_id);
                    Some(json!({ "v": PROTOCOL_VERSION, "type": "answer", "sdp": sdp_answer }))
                }
                Ok(AgentResponse::Error { code, message }) => {
                    warn!(%code, %message, "offer refused");
                    Some(error_message(&code))
                }
                Ok(other) => {
                    warn!(?other, "unexpected agent reply");
                    Some(error_message("agent_error"))
                }
                Err(e) => {
                    warn!(error = %e, "agent unavailable");
                    Some(error_message("agent_unavailable"))
                }
            }
        }
        // The client sends a new offer right after; the old session ends when that one opens.
        Some("restart") => None,
        _ => Some(error_message("bad_message")),
    }
}
