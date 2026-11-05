//! Sign-in: sessions, attempt limits, the `/api/auth/*` handlers and the guard in front of every `/api` route
//! (docs/PROTOCOL.md, docs/SECURITY.md).

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::error;

use crate::app::AppState;
use crate::secrets::{AuthFile, MAX_PASSWORD_BYTES, verify_password};
use crate::totp;
use crate::util::{hex, random, rfc3339, sha256, unix_seconds};

pub const COOKIE: &str = "__Secure-rcs_session";
const TOKEN_HEX: usize = 64;

#[derive(Clone, Debug)]
pub struct Policy {
    pub pending_ttl: Duration,
    pub idle_ttl: Duration,
    pub absolute_ttl: Duration,
    pub ip_failures: usize,
    pub ip_window: Duration,
    pub ip_block: Duration,
    pub global_failures: usize,
    pub global_window: Duration,
    pub global_block: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            pending_ttl: Duration::from_secs(5 * 60),
            idle_ttl: Duration::from_secs(60 * 60),
            absolute_ttl: Duration::from_secs(12 * 60 * 60),
            ip_failures: 5,
            ip_window: Duration::from_secs(15 * 60),
            ip_block: Duration::from_secs(15 * 60),
            global_failures: 20,
            global_window: Duration::from_secs(60 * 60),
            global_block: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Password accepted, waiting for TOTP or a recovery code.
    Pending,
    Full,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Pending => "pending",
            Stage::Full => "full",
        }
    }
}

type Key = [u8; 32];

struct Entry {
    stage: Stage,
    created: Instant,
    last_seen: Instant,
    /// Set to true when the session ends, so its WebSocket closes at once.
    ended: watch::Sender<bool>,
}

/// What a request learns about its session.
#[derive(Clone, Debug)]
pub struct SessionRef {
    pub key: Key,
    pub stage: Stage,
    pub ended: watch::Receiver<bool>,
    pub expires_in: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    /// It ran out just now and was removed.
    Expired,
    /// Already gone: logged out, replaced or expired earlier.
    Gone,
}

pub struct Sessions {
    policy: Policy,
    map: Mutex<HashMap<Key, Entry>>,
}

fn key_of(token: &str) -> Option<Key> {
    (token.len() == TOKEN_HEX && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| sha256(token.as_bytes()))
}

impl Sessions {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            map: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remaining(&self, e: &Entry, now: Instant) -> Option<Duration> {
        match e.stage {
            Stage::Pending => (e.created + self.policy.pending_ttl).checked_duration_since(now),
            Stage::Full => {
                let absolute =
                    (e.created + self.policy.absolute_ttl).checked_duration_since(now)?;
                let idle = (e.last_seen + self.policy.idle_ttl).checked_duration_since(now)?;
                Some(absolute.min(idle))
            }
        }
        .filter(|d| !d.is_zero())
    }

    fn reference(&self, key: Key, e: &Entry, now: Instant) -> SessionRef {
        SessionRef {
            key,
            stage: e.stage,
            ended: e.ended.subscribe(),
            expires_in: self.remaining(e, now).unwrap_or_default(),
        }
    }

    /// A new session; the token goes into the cookie and only its SHA-256 stays here.
    pub fn create(&self, stage: Stage, now: Instant) -> (String, SessionRef) {
        let token = hex(&random::<32>());
        let key = sha256(token.as_bytes());
        let entry = Entry {
            stage,
            created: now,
            last_seen: now,
            ended: watch::channel(false).0,
        };
        let reference = self.reference(key, &entry, now);
        self.lock().insert(key, entry);
        (token, reference)
    }

    /// The live session of `token`; a full session counts this as activity.
    pub fn lookup(&self, token: &str, now: Instant) -> Option<SessionRef> {
        let key = key_of(token)?;
        let mut map = self.lock();
        if self.remaining(map.get(&key)?, now).is_none() {
            if let Some(e) = map.remove(&key) {
                e.ended.send_replace(true);
            }
            return None;
        }
        let entry = map.get_mut(&key)?;
        entry.last_seen = now;
        Some(self.reference(key, entry, now))
    }

    /// For an open WebSocket: checks the session and, with an active stream, extends its idle time.
    pub fn alive(&self, key: &Key, extend: bool, now: Instant) -> Liveness {
        let mut map = self.lock();
        let Some(entry) = map.get(key) else {
            return Liveness::Gone;
        };
        if self.remaining(entry, now).is_none() {
            if let Some(e) = map.remove(key) {
                e.ended.send_replace(true);
            }
            return Liveness::Expired;
        }
        if extend && let Some(entry) = map.get_mut(key) {
            entry.last_seen = now;
        }
        Liveness::Alive
    }

    pub fn end_token(&self, token: &str) -> Option<Stage> {
        let key = key_of(token)?;
        let e = self.lock().remove(&key)?;
        e.ended.send_replace(true);
        Some(e.stage)
    }

    /// Replaces a pending session with a full one under a new token and ends every other session.
    /// Returns the token, the session and how many full sessions were ended.
    pub fn promote(&self, pending: &Key, now: Instant) -> Option<(String, SessionRef, usize)> {
        let mut map = self.lock();
        let entry = map.get(pending)?;
        if entry.stage != Stage::Pending || self.remaining(entry, now).is_none() {
            return None;
        }
        let mut replaced = 0;
        for (_, e) in map.drain() {
            if e.stage == Stage::Full {
                replaced += 1;
            }
            e.ended.send_replace(true);
        }
        drop(map);
        let (token, reference) = self.create(Stage::Full, now);
        Some((token, reference, replaced))
    }

    /// Removes expired sessions; returns the stages of those removed.
    pub fn sweep(&self, now: Instant) -> Vec<Stage> {
        let mut map = self.lock();
        let expired: Vec<Key> = map
            .iter()
            .filter(|(_, e)| self.remaining(e, now).is_none())
            .map(|(k, _)| *k)
            .collect();
        expired
            .into_iter()
            .filter_map(|k| map.remove(&k))
            .map(|e| {
                e.ended.send_replace(true);
                e.stage
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Ip,
    All,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Ip => "ip",
            Scope::All => "all",
        }
    }
}

#[derive(Default)]
struct IpFailures {
    times: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

#[derive(Default)]
struct Attempts {
    per_ip: HashMap<IpAddr, IpFailures>,
    global: VecDeque<Instant>,
    global_blocked_until: Option<Instant>,
}

/// Failed sign-in attempts: per IP and in total (docs/SECURITY.md, Ограничение попыток).
pub struct Limiter {
    policy: Policy,
    state: Mutex<Attempts>,
}

fn prune(times: &mut VecDeque<Instant>, window: Duration, now: Instant) {
    while times
        .front()
        .is_some_and(|t| now.duration_since(*t) >= window)
    {
        times.pop_front();
    }
}

impl Limiter {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            state: Mutex::new(Attempts::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Attempts> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How long `ip` has to wait, if it is blocked now.
    pub fn blocked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let s = self.lock();
        let wait = |until: Option<Instant>| {
            until
                .and_then(|u| u.checked_duration_since(now))
                .filter(|d| !d.is_zero())
        };
        let ip_wait = wait(s.per_ip.get(&ip).and_then(|f| f.blocked_until));
        let all_wait = wait(s.global_blocked_until);
        ip_wait.max(all_wait)
    }

    /// Counts a failure; returns the block it started, if any.
    pub fn failure(&self, ip: IpAddr, now: Instant) -> Option<Scope> {
        let mut s = self.lock();
        let mut started = None;
        prune(&mut s.global, self.policy.global_window, now);
        s.global.push_back(now);
        if s.global.len() >= self.policy.global_failures
            && s.global_blocked_until.is_none_or(|u| u <= now)
        {
            s.global_blocked_until = Some(now + self.policy.global_block);
            s.global.clear();
            started = Some(Scope::All);
        }
        let policy = self.policy.clone();
        let f = s.per_ip.entry(ip).or_default();
        prune(&mut f.times, policy.ip_window, now);
        f.times.push_back(now);
        if f.times.len() >= policy.ip_failures && f.blocked_until.is_none_or(|u| u <= now) {
            f.blocked_until = Some(now + policy.ip_block);
            f.times.clear();
            started = started.or(Some(Scope::Ip));
        }
        // Forget addresses with nothing left to remember.
        s.per_ip
            .retain(|_, f| !f.times.is_empty() || f.blocked_until.is_some_and(|u| u > now));
        started
    }

    pub fn success(&self, ip: IpAddr) {
        self.lock().per_ip.remove(&ip);
    }
}

/// The address of the browser. Behind Caddy the connection comes from loopback and Caddy appends the real
/// address to `X-Forwarded-For`; from anywhere else that header is ignored.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
    let peer = peer.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |p| p.ip());
    if peer.is_loopback()
        && let Some(forwarded) = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .next_back()
        && let Ok(ip) = forwarded.trim().parse::<IpAddr>()
    {
        return ip;
    }
    peer
}

#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

pub fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.trim())
}

fn set_cookie(path: &str, token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path={path}; Secure; HttpOnly; SameSite=Strict"
    ))
    .expect("the token is hex")
}

fn clear_cookie(path: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}=; Path={path}; Secure; HttpOnly; SameSite=Strict; Max-Age=0"
    ))
    .expect("the path is ASCII")
}

pub fn json_error(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(json!({ "error": code }))).into_response()
}

fn too_many(wait: Duration) -> Response {
    let mut r = json_error(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    r
}

fn field(body: &[u8], name: &str) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text = value.get(name)?.as_str()?;
    (text.len() <= MAX_PASSWORD_BYTES).then(|| text.to_string())
}

fn expires_at(reference: &SessionRef) -> String {
    rfc3339(SystemTime::now() + reference.expires_in)
}

/// One sign-in check at a time. The block is checked, `f` runs on the secrets file and a failure is counted under
/// the same lock, so parallel requests cannot slip extra guesses past the limit. `f` returns the value on success
/// (None is a wrong password or code) and whether the file changed.
#[expect(
    clippy::result_large_err,
    reason = "the error is the finished HTTP reply, made once per sign-in request"
)]
async fn attempt<T: Send + 'static>(
    state: &Arc<AppState>,
    ip: IpAddr,
    step: &'static str,
    f: impl FnOnce(&mut AuthFile) -> Result<(Option<T>, bool), String> + Send + 'static,
) -> Result<T, Response> {
    let _turn = state.store_lock.lock().await;
    if let Some(wait) = state.limiter.blocked(ip, Instant::now()) {
        return Err(too_many(wait));
    }
    let s = state.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let mut file = s.store.load()?;
        let (value, changed) = f(&mut file)?;
        if changed {
            s.store.save(&file)?;
        }
        Ok(value)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    match checked {
        Err(e) => Err(server_error(&e)),
        Ok(None) => Err(failed(state, ip, step)),
        Ok(Some(value)) => Ok(value),
    }
}

fn failed(state: &AppState, ip: IpAddr, step: &str) -> Response {
    let block = state.limiter.failure(ip, Instant::now());
    state
        .audit
        .event("login_failed", Some(ip), json!({ "step": step }));
    if let Some(scope) = block {
        state.audit.event(
            "login_blocked",
            Some(ip),
            json!({ "scope": scope.as_str(), "seconds": match scope {
                Scope::Ip => state.policy.ip_block.as_secs(),
                Scope::All => state.policy.global_block.as_secs(),
            } }),
        );
    }
    json_error(StatusCode::UNAUTHORIZED, "invalid_credentials")
}

fn server_error(e: &str) -> Response {
    error!(error = %e, "sign-in failed on the server");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    axum::Extension(ClientIp(ip)): axum::Extension<ClientIp>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(wait) = state.limiter.blocked(ip, Instant::now()) {
        return too_many(wait);
    }
    let Some(password) = field(&body, "password") else {
        return json_error(StatusCode::BAD_REQUEST, "bad_request");
    };
    let checked = attempt(&state, ip, "password", move |f| {
        let ok = f
            .password_hash
            .as_deref()
            .is_some_and(|h| verify_password(h, &password));
        Ok((ok.then_some(()), false))
    })
    .await;
    match checked {
        Err(r) => r,
        Ok(()) => {
            // A browser that signs in again drops its earlier pending session.
            if let Some(old) = cookie_token(&headers)
                && state
                    .sessions
                    .lookup(old, Instant::now())
                    .is_some_and(|r| r.stage == Stage::Pending)
            {
                state.sessions.end_token(old);
            }
            let (token, _) = state.sessions.create(Stage::Pending, Instant::now());
            state.audit.event("password_accepted", Some(ip), json!({}));
            let mut r = axum::Json(json!({ "next": "totp" })).into_response();
            r.headers_mut()
                .insert(header::SET_COOKIE, set_cookie(&state.cookie_path, &token));
            r
        }
    }
}

fn pending_session(state: &AppState, headers: &HeaderMap) -> Option<SessionRef> {
    let r = state
        .sessions
        .lookup(cookie_token(headers)?, Instant::now())?;
    (r.stage == Stage::Pending).then_some(r)
}

/// The second step: on success the pending session becomes the only full one.
async fn second_step(
    state: Arc<AppState>,
    ip: IpAddr,
    pending: SessionRef,
    method: &'static str,
    checked: Result<Value, Response>,
) -> Response {
    match checked {
        Err(r) => r,
        Ok(extra) => {
            let Some((token, session, replaced)) =
                state.sessions.promote(&pending.key, Instant::now())
            else {
                return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
            };
            state.limiter.success(ip);
            let mut fields = json!({ "method": method });
            if let (Value::Object(f), Value::Object(e)) = (&mut fields, extra) {
                f.extend(e);
            }
            state.audit.event("login_succeeded", Some(ip), fields);
            if replaced > 0 {
                state
                    .audit
                    .event("session_replaced", Some(ip), json!({ "ended": replaced }));
            }
            let mut r =
                axum::Json(json!({ "authenticated": true, "expires_at": expires_at(&session) }))
                    .into_response();
            r.headers_mut()
                .insert(header::SET_COOKIE, set_cookie(&state.cookie_path, &token));
            r
        }
    }
}

pub async fn totp(
    State(state): State<Arc<AppState>>,
    axum::Extension(ClientIp(ip)): axum::Extension<ClientIp>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(wait) = state.limiter.blocked(ip, Instant::now()) {
        return too_many(wait);
    }
    let Some(pending) = pending_session(&state, &headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let Some(code) = field(&body, "code") else {
        return json_error(StatusCode::BAD_REQUEST, "bad_request");
    };
    let now_step = totp::step_of(unix_seconds(SystemTime::now()));
    let checked = attempt(&state, ip, "totp", move |f| {
        let secret = f.totp_secret()?;
        Ok(
            match totp::verify(&secret, &code, now_step, f.totp_last_step) {
                Some(step) => {
                    f.totp_last_step = Some(step);
                    (Some(json!({})), true)
                }
                None => (None, false),
            },
        )
    })
    .await;
    second_step(state, ip, pending, "totp", checked).await
}

pub async fn recovery(
    State(state): State<Arc<AppState>>,
    axum::Extension(ClientIp(ip)): axum::Extension<ClientIp>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(wait) = state.limiter.blocked(ip, Instant::now()) {
        return too_many(wait);
    }
    let Some(pending) = pending_session(&state, &headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let Some(code) = field(&body, "code") else {
        return json_error(StatusCode::BAD_REQUEST, "bad_request");
    };
    let checked = attempt(&state, ip, "recovery", move |f| {
        Ok(if f.take_recovery_code(&code) {
            (
                Some(json!({ "recovery_left": f.recovery_sha256.len() })),
                true,
            )
        } else {
            (None, false)
        })
    })
    .await;
    second_step(state, ip, pending, "recovery", checked).await
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    axum::Extension(ClientIp(ip)): axum::Extension<ClientIp>,
    headers: HeaderMap,
) -> Response {
    if let Some(stage) = cookie_token(&headers).and_then(|t| state.sessions.end_token(t)) {
        state
            .audit
            .event("logout", Some(ip), json!({ "stage": stage.as_str() }));
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, clear_cookie(&state.cookie_path));
    r
}

pub async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match cookie_token(&headers).and_then(|t| state.sessions.lookup(t, Instant::now())) {
        Some(r) if r.stage == Stage::Full => {
            axum::Json(json!({ "authenticated": true, "expires_at": expires_at(&r) }))
                .into_response()
        }
        Some(_) => (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "unauthorized", "next": "totp" })),
        )
            .into_response(),
        None => json_error(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
}

/// Paths under `/api` that work without a full session; the handlers check the pending session themselves.
fn is_sign_in(path: &str) -> bool {
    matches!(
        path,
        "/auth/login" | "/auth/totp" | "/auth/recovery" | "/auth/logout" | "/auth/me"
    )
}

/// In front of every `/api` route: a full session, then `Origin` and `X-RCS-Request` for anything that changes
/// state or opens a WebSocket.
pub async fn guard(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    // Tests put the address in MockConnectInfo, which the ConnectInfo extractor also reads.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .or_else(|| {
            req.extensions()
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|m| m.0)
        });
    let ip = client_ip(req.headers(), peer);
    req.extensions_mut().insert(ClientIp(ip));
    if !is_sign_in(req.uri().path()) {
        let session =
            cookie_token(req.headers()).and_then(|t| state.sessions.lookup(t, Instant::now()));
        match session {
            Some(r) if r.stage == Stage::Full => {
                req.extensions_mut().insert(r);
            }
            _ => return json_error(StatusCode::UNAUTHORIZED, "unauthorized"),
        }
    }
    let changes_state = !matches!(*req.method(), Method::GET | Method::HEAD);
    let websocket = req.headers().contains_key(header::UPGRADE);
    match req.headers().get(header::ORIGIN) {
        Some(origin) if origin.as_bytes() != state.origin.as_bytes() => {
            return json_error(StatusCode::FORBIDDEN, "forbidden");
        }
        None if changes_state || websocket => {
            return json_error(StatusCode::FORBIDDEN, "forbidden");
        }
        _ => {}
    }
    if changes_state
        && req
            .headers()
            .get("x-rcs-request")
            .map(HeaderValue::as_bytes)
            != Some(b"1")
    {
        return json_error(StatusCode::FORBIDDEN, "forbidden");
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::default()
    }

    #[test]
    fn sessions_expire_by_stage() {
        let p = policy();
        let s = Sessions::new(p.clone());
        let t0 = Instant::now();
        let (pending, _) = s.create(Stage::Pending, t0);
        assert!(
            s.lookup(&pending, t0 + p.pending_ttl - Duration::from_secs(1))
                .is_some()
        );
        assert!(s.lookup(&pending, t0 + p.pending_ttl).is_none());
        // Gone for good once expired.
        assert!(s.lookup(&pending, t0).is_none());

        let (full, r) = s.create(Stage::Full, t0);
        // Activity keeps it, but not past the absolute limit.
        let mut t = t0;
        while t + p.idle_ttl / 2 < t0 + p.absolute_ttl {
            t += p.idle_ttl / 2;
            assert_eq!(s.alive(&r.key, true, t), Liveness::Alive);
        }
        assert_eq!(
            s.alive(&r.key, true, t0 + p.absolute_ttl),
            Liveness::Expired
        );
        assert!(s.lookup(&full, t0).is_none());

        let (_, idle) = s.create(Stage::Full, t0);
        assert_eq!(
            s.alive(&idle.key, false, t0 + p.idle_ttl),
            Liveness::Expired
        );
        assert_eq!(s.alive(&idle.key, false, t0 + p.idle_ttl), Liveness::Gone);
    }

    #[test]
    fn promotion_ends_everything_else() {
        let s = Sessions::new(policy());
        let now = Instant::now();
        let (old, old_ref) = s.create(Stage::Full, now);
        let (pending, pending_ref) = s.create(Stage::Pending, now);
        let (token, session, replaced) = s.promote(&pending_ref.key, now).unwrap();
        assert_eq!(replaced, 1);
        assert!(*old_ref.ended.borrow());
        assert!(s.lookup(&old, now).is_none());
        assert!(s.lookup(&pending, now).is_none());
        assert_eq!(s.lookup(&token, now).unwrap().stage, Stage::Full);
        assert!(!*session.ended.borrow());
        // A full session cannot be promoted, a vanished one neither.
        assert!(s.promote(&session.key, now).is_none());
        assert!(s.promote(&pending_ref.key, now).is_none());
    }

    #[test]
    fn malformed_tokens_are_ignored() {
        let s = Sessions::new(policy());
        assert!(s.lookup("", Instant::now()).is_none());
        assert!(s.lookup(&"z".repeat(TOKEN_HEX), Instant::now()).is_none());
        assert!(s.end_token("abc").is_none());
    }

    #[test]
    fn limiter_blocks_an_ip_then_everyone() {
        let p = policy();
        let l = Limiter::new(p.clone());
        let t0 = Instant::now();
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        for i in 0..p.ip_failures - 1 {
            assert_eq!(l.failure(a, t0 + Duration::from_secs(i as u64)), None);
            assert!(l.blocked(a, t0).is_none());
        }
        assert_eq!(l.failure(a, t0 + Duration::from_secs(10)), Some(Scope::Ip));
        let wait = l.blocked(a, t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(wait, p.ip_block);
        assert!(
            l.blocked(a, t0 + Duration::from_secs(10) + p.ip_block)
                .is_none()
        );

        // Failures spread over many addresses: the 20th in an hour blocks every new sign-in.
        let l = Limiter::new(p.clone());
        let mut started = None;
        for i in 0..p.global_failures {
            let ip: IpAddr = format!("198.51.100.{i}").parse().unwrap();
            started = l.failure(ip, t0 + Duration::from_secs(i as u64));
        }
        assert_eq!(started, Some(Scope::All));
        assert!(
            l.blocked("203.0.113.9".parse().unwrap(), t0 + Duration::from_secs(60))
                .is_some()
        );
    }

    #[test]
    fn failures_outside_the_window_do_not_count() {
        let p = policy();
        let l = Limiter::new(p.clone());
        let a: IpAddr = "192.0.2.2".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..p.ip_failures - 1 {
            l.failure(a, t0 + Duration::from_secs(i as u64));
        }
        assert_eq!(
            l.failure(a, t0 + p.ip_window + Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn forwarded_address_is_trusted_only_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("10.0.0.1, 203.0.113.5"),
        );
        let local: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let remote: SocketAddr = "192.0.2.7:50000".parse().unwrap();
        assert_eq!(
            client_ip(&h, Some(local)),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(&h, Some(remote)), remote.ip());
        assert_eq!(client_ip(&HeaderMap::new(), Some(local)), local.ip());
    }

    #[test]
    fn cookie_is_found_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __Secure-rcs_session=abc ; b=2"),
        );
        assert_eq!(cookie_token(&h), Some("abc"));
        assert_eq!(cookie_token(&HeaderMap::new()), None);
    }
}
