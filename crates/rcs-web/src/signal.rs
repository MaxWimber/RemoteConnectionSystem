//! `GET /api/signal`: WebSocket signaling from docs/PROTOCOL.md. Only a full session gets here (`auth::guard`).
//!
//! The socket lives as long as its sign-in session: logout, a new sign-in and expiry close it at once and end the
//! media session in the agent. An active stream counts as activity for the idle limit.

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
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::app::{AppState, Owner};
use crate::auth::{ClientIp, Liveness, SessionRef};

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
    info!(socket = id, "signaling connected");
    while signed_in {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(frame)) = incoming else { break };
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let reply = handle(&state, id, &text, &notify, &mut session, ip).await;
                if let Some(reply) = reply
                    && socket.send(Message::Text(reply.to_string().into())).await.is_err()
                {
                    break;
                }
            }
            Some(notice) = notices.recv() => {
                // Another browser took the media session; this one keeps the socket but has no media.
                session = None;
                if socket.send(Message::Text(notice.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = ended.changed() => signed_in = false,
            _ = check.tick() => match state.sessions.alive(&auth.key, session.is_some(), Instant::now()) {
                Liveness::Alive => {}
                Liveness::Expired => {
                    state.audit.event("session_expired", Some(ip), json!({}));
                    signed_in = false;
                }
                Liveness::Gone => signed_in = false,
            },
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
    // A browser that leaves ends its media session, unless another browser already replaced it.
    if let Some(session_id) = session {
        let mut owner = state.owner.lock().await;
        if owner.as_ref().is_some_and(|o| o.session_id == session_id) {
            *owner = None;
            drop(owner);
            let _ = state.agent.call(Request::CloseSession { session_id }).await;
            state.audit.event("stream_stopped", Some(ip), json!({}));
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
        _ => Some(error_message("bad_message")),
    }
}
