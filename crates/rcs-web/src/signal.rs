//! `GET /api/signal`: WebSocket signaling from docs/PROTOCOL.md. Only a full session gets here (`auth::guard`).
//!
//! The socket lives as long as its sign-in session: logout, a new sign-in and expiry close it at once and end the
//! media session in the agent. An active stream counts as activity for the idle limit. A socket that was lost, not
//! closed, leaves its media session waiting for the same sign-in to take it back with `resume` (D-031).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use rcs_protocol::PROTOCOL_VERSION;
use rcs_protocol::agent::{Request, Response as AgentResponse};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::app::{AppState, Owner};
use crate::auth::{ClientIp, Key, Liveness, SessionRef};

const CHECK: Duration = Duration::from_secs(1);
/// Close code for a session that ended, so the browser goes back to sign-in instead of reconnecting.
pub const CLOSE_SESSION_ENDED: u16 = 4401;

pub async fn signal(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionRef>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
) -> Response {
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| serve_socket(socket, state, session, ip))
}

fn message(kind: &str) -> Value {
    json!({ "v": PROTOCOL_VERSION, "type": kind })
}

fn error_message(code: &str) -> Value {
    json!({ "v": PROTOCOL_VERSION, "type": "error", "code": code })
}

async fn serve_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    auth: SessionRef,
    ip: std::net::IpAddr,
) {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let (notify, mut notices) = mpsc::channel::<Value>(4);
    let mut session: Option<String> = None;
    let mut ended = auth.ended.clone();
    let mut check = tokio::time::interval(CHECK);
    let mut signed_in = !*ended.borrow();
    let opened = Instant::now();
    let mut heard = opened;
    let mut pinged = opened;
    // Why the socket ended, for the log: a lost connection and a closed tab look the same to the browser (1006).
    let mut why = String::from("session ended");
    // The connection was lost rather than closed by the browser: the media session may be taken back (D-031).
    let mut lost = false;
    info!(socket = id, "signaling connected");
    while signed_in {
        tokio::select! {
            incoming = socket.recv() => {
                let frame = match incoming {
                    Some(Ok(frame)) => frame,
                    Some(Err(e)) => {
                        why = format!("connection lost: {e}");
                        lost = true;
                        break;
                    }
                    None => {
                        why = "connection lost".into();
                        lost = true;
                        break;
                    }
                };
                heard = Instant::now();
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(c) => {
                        why = match c {
                            Some(c) => format!("closed by the browser ({})", c.code),
                            None => "closed by the browser".into(),
                        };
                        break;
                    }
                    _ => continue,
                };
                let reply = handle(&state, id, &auth.key, &text, &notify, &mut session, ip).await;
                if let Some(reply) = reply
                    && let Err(e) = socket.send(Message::Text(reply.to_string().into())).await
                {
                    why = format!("send failed: {e}");
                    lost = true;
                    break;
                }
            }
            Some(notice) = notices.recv() => {
                // Another browser took the media session; this one keeps the socket but has no media.
                session = None;
                if let Err(e) = socket.send(Message::Text(notice.to_string().into())).await {
                    why = format!("send failed: {e}");
                    lost = true;
                    break;
                }
            }
            _ = ended.changed() => signed_in = false,
            _ = check.tick() => {
                let now = Instant::now();
                match state.sessions.alive(&auth.key, session.is_some(), now) {
                    Liveness::Alive => {}
                    Liveness::Expired => {
                        state.audit.event("session_expired", Some(ip), json!({}));
                        signed_in = false;
                        continue;
                    }
                    Liveness::Gone => {
                        signed_in = false;
                        continue;
                    }
                }
                if now.duration_since(heard) >= state.policy.socket_silence {
                    why = "no answer to ping".into();
                    lost = true;
                    break;
                }
                let ping = state.policy.socket_ping;
                if now.duration_since(heard) >= ping && now.duration_since(pinged) >= ping {
                    pinged = now;
                    if let Err(e) = socket.send(Message::Ping(Vec::new().into())).await {
                        why = format!("send failed: {e}");
                        lost = true;
                        break;
                    }
                }
            }
        }
    }
    if !signed_in {
        let _ = socket
            .send(Message::Text(
                error_message("session_ended").to_string().into(),
            ))
            .await;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_SESSION_ENDED,
                reason: "session ended".into(),
            })))
            .await;
    }
    // A browser that leaves ends its media session, unless another socket already holds it. A lost socket leaves it
    // waiting for `resume` (D-031).
    if let Some(session_id) = session {
        let mut owner = state.owner.lock().await;
        if let Some(o) = owner
            .as_mut()
            .filter(|o| o.session_id == session_id && o.socket == id)
        {
            if lost && signed_in {
                o.orphaned = true;
                drop(owner);
                info!(socket = id, "media session waits for resume");
                tokio::spawn(close_unless_resumed(
                    state.clone(),
                    session_id,
                    id,
                    auth.ended.clone(),
                    ip,
                ));
            } else {
                *owner = None;
                drop(owner);
                close_media(&state, session_id, ip).await;
            }
        }
    }
    let seconds = opened.elapsed().as_secs_f64();
    info!(socket = id, why, seconds, "signaling closed");
}

async fn close_media(state: &AppState, session_id: String, ip: std::net::IpAddr) {
    let _ = state.agent.call(Request::CloseSession { session_id }).await;
    state.audit.event("stream_stopped", Some(ip), json!({}));
}

/// Ends a media session that lost its socket, unless a socket of the same sign-in took it back in time. The end of
/// the sign-in session ends it at once.
async fn close_unless_resumed(
    state: Arc<AppState>,
    session_id: String,
    socket: u64,
    mut ended: watch::Receiver<bool>,
    ip: std::net::IpAddr,
) {
    if !*ended.borrow() {
        tokio::select! {
            _ = tokio::time::sleep(state.policy.resume_grace) => {}
            // Also when the sign-in session is gone and its sender with it.
            _ = ended.changed() => {}
        }
    }
    let mut owner = state.owner.lock().await;
    if owner
        .as_ref()
        .is_some_and(|o| o.session_id == session_id && o.socket == socket)
    {
        *owner = None;
        drop(owner);
        info!(socket, "no resume, media session closed");
        close_media(&state, session_id, ip).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    state: &AppState,
    socket_id: u64,
    login: &Key,
    text: &str,
    notify: &mpsc::Sender<Value>,
    session: &mut Option<String>,
    ip: std::net::IpAddr,
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
                        socket: socket_id,
                        login: *login,
                        orphaned: false,
                    });
                    if let Some(previous) = previous
                        && previous.session_id != session_id
                        && !previous.notify.same_channel(notify)
                    {
                        let _ = previous.notify.try_send(message("session-replaced"));
                    }
                    if session.is_none() {
                        state.audit.event("stream_started", Some(ip), json!({}));
                    }
                    *session = Some(session_id);
                    Some(json!({ "v": PROTOCOL_VERSION, "type": "answer", "sdp": sdp_answer }))
                }
                Ok(AgentResponse::Error { code, message }) => {
                    warn!(%code, %message, "offer refused");
                    Some(error_message(&code))
                }
                Ok(_) => {
                    warn!("unexpected agent reply");
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
        // A new socket after a lost one: the media session goes on if this sign-in opened it (D-031).
        Some("resume") => {
            let mut owner = state.owner.lock().await;
            match owner.as_mut() {
                Some(o) if o.orphaned && o.login == *login => {
                    o.orphaned = false;
                    o.socket = socket_id;
                    o.notify = notify.clone();
                    *session = Some(o.session_id.clone());
                    info!(socket = socket_id, "media session resumed");
                    state.audit.event("stream_resumed", Some(ip), json!({}));
                    Some(message("resumed"))
                }
                _ => Some(error_message("resume_failed")),
            }
        }
        _ => Some(error_message("bad_message")),
    }
}
