//! Shared state and the HTTP routes of `rcs-web`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rcs_protocol::agent::{Request, Response as AgentResponse};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::agent;
use crate::audit::Audit;
use crate::auth::{self, Limiter, Policy, Sessions, Stage, json_error};
use crate::files::{self, Files};
use crate::secrets::Store;
use crate::signal;

/// The browser that owns the media session, so a new one can tell the old one it was replaced.
pub struct Owner {
    pub session_id: String,
    pub notify: mpsc::Sender<Value>,
}

pub struct AppState {
    pub agent: agent::Client,
    pub owner: Mutex<Option<Owner>>,
    pub next_id: AtomicU64,
    pub sessions: Sessions,
    pub limiter: Limiter,
    pub policy: Policy,
    pub store: Store,
    /// One sign-in check at a time (`auth::attempt`): it reads and writes the secrets file, and one Argon2 run
    /// (19 MiB, tens of milliseconds) at a time keeps a flood of attempts from taking the machine.
    pub store_lock: Mutex<()>,
    pub audit: Audit,
    /// The only `Origin` accepted, e.g. `https://maxwimber.ru:8444`.
    pub origin: String,
    /// `Path` of the cookie: the base path with a trailing slash.
    pub cookie_path: String,
    pub files: Files,
}

pub struct Settings {
    pub pipe: String,
    pub base: String,
    pub origin: String,
    pub policy: Policy,
    pub store: Store,
    pub audit: Audit,
    pub files: Files,
}

impl AppState {
    pub fn new(s: Settings) -> Self {
        Self {
            agent: agent::Client::new(s.pipe),
            owner: Mutex::new(None),
            next_id: AtomicU64::new(1),
            sessions: Sessions::new(s.policy.clone()),
            limiter: Limiter::new(s.policy.clone()),
            policy: s.policy,
            store: s.store,
            store_lock: Mutex::new(()),
            audit: s.audit,
            origin: s.origin,
            cookie_path: format!("{}/", s.base),
            files: s.files,
        }
    }
}

/// `connect-src` needs the WebSocket form of the origin.
fn content_security_policy(origin: &str) -> String {
    let ws = origin
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; \
         connect-src 'self' {ws}; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
    )
}

pub fn router(state: Arc<AppState>, static_dir: &Path, base: &str) -> Router {
    let csp = HeaderValue::from_str(&content_security_policy(&state.origin)).expect("ASCII origin");
    let index = static_dir.join("index.html");
    let client = ServeDir::new(static_dir).fallback(ServeFile::new(&index));
    let api = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/totp", post(auth::totp))
        .route("/auth/recovery", post(auth::recovery))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/signal", get(signal::signal))
        .route("/status", get(status))
        .route("/health", get(|| async { "ok" }))
        .merge(files::routes())
        .fallback(|| async { json_error(StatusCode::NOT_FOUND, "not_found") })
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state);
    let site = Router::new().nest("/api", api).fallback_service(client);
    let base_owned = base.to_string();
    // A nested router does not match its own path with a trailing slash, which is the client's address.
    Router::new()
        .route_service(&format!("{base}/"), ServeFile::new(&index))
        .nest(base, site)
        .route(
            "/",
            get(move || async move { Redirect::temporary(&format!("{base_owned}/")) }),
        )
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=()"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
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
        ))
}

/// Ends expired sessions that have no open WebSocket to notice it.
pub fn spawn_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            for stage in state.sessions.sweep(Instant::now()) {
                if stage == Stage::Full {
                    state.audit.event("session_expired", None, json!({}));
                }
            }
        }
    });
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    match state.agent.call(Request::Status).await {
        Ok(AgentResponse::AgentStatus(s)) => axum::Json(s).into_response(),
        Ok(_) => json_error(StatusCode::BAD_GATEWAY, "agent_error"),
        Err(_) => json_error(StatusCode::SERVICE_UNAVAILABLE, "agent_unavailable"),
    }
}
