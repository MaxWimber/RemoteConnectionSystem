//! P3-8: bypassing sign-in, reusing a code, attempt limits and the WebSocket lifetime, against the real router.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tower::ServiceExt;

use crate::app::{self, AppState, Settings};
use crate::audit::Audit;
use crate::auth::{COOKIE, Policy};
use crate::secrets::tests::temp_dir;
use crate::secrets::{AuthFile, Store, hash_password, new_recovery_code};
use crate::totp;
use crate::util::unix_seconds;

const PASSWORD: &str = "a test password, long enough";
const ORIGIN: &str = "https://rcs.test:8444";
const BASE: &str = "/RemoteConnectionSystem";

struct Fixture {
    state: Arc<AppState>,
    router: Router,
    www: PathBuf,
    secret: Vec<u8>,
    codes: Vec<String>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(policy: Policy) -> Fixture {
    let dir = temp_dir("app");
    let store = Store::new(dir.join("secrets"));
    store.prepare().unwrap();
    let secret = vec![7u8; totp::SECRET_BYTES];
    let codes: Vec<String> = (0..8).map(|_| new_recovery_code()).collect();
    let mut file = AuthFile {
        password_hash: Some(hash_password(PASSWORD).unwrap()),
        ..AuthFile::default()
    };
    file.set_totp_secret(&secret).unwrap();
    file.set_recovery_codes(&codes);
    store.save(&file).unwrap();
    let storage = dir.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let www = dir.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "<!doctype html><title>t</title>").unwrap();
    let state = Arc::new(AppState::new(Settings {
        pipe: format!(
            r"\\.\pipe\rcs-web-test-{}",
            crate::util::hex(&crate::util::random::<6>())
        ),
        base: BASE.into(),
        origin: ORIGIN.into(),
        policy,
        store,
        audit: Audit::open(dir.join("audit.log")).unwrap(),
        files: crate::files::Files::new(&storage, None, 1 << 30),
    }));
    // Requests come "from Caddy": loopback, with the browser address in X-Forwarded-For.
    let router = app::router(state.clone(), &www, BASE)
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    Fixture {
        state,
        router,
        www,
        secret,
        codes,
        dir,
    }
}

fn code(f: &Fixture, offset: i64) -> String {
    let now = totp::step_of(unix_seconds(SystemTime::now()));
    totp::format_code(totp::hotp(
        &f.secret,
        now.saturating_add_signed(offset),
        totp::DIGITS,
    ))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn cookie(&self) -> Option<String> {
        let set = self.headers.get(header::SET_COOKIE)?.to_str().ok()?;
        let pair = set.split(';').next()?;
        let (name, value) = pair.split_once('=')?;
        (name == COOKIE && !value.is_empty()).then(|| value.to_string())
    }
}

async fn send(f: &Fixture, req: Request<Body>) -> Reply {
    let r = f.router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    Reply {
        status,
        headers,
        body,
    }
}

fn post(path: &str, body: Value, cookie: Option<&str>, ip: &str) -> Request<Body> {
    let mut b = Request::post(format!("{BASE}/api{path}"))
        .header(header::ORIGIN, ORIGIN)
        .header("x-rcs-request", "1")
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, format!("{COOKIE}={c}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(format!("{BASE}/api{path}")).header("x-forwarded-for", "192.0.2.50");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, format!("{COOKIE}={c}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn password_step(f: &Fixture, ip: &str) -> String {
    let r = send(
        f,
        post("/auth/login", json!({ "password": PASSWORD }), None, ip),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["next"], "totp");
    r.cookie().expect("pending cookie")
}

/// Signs in with the TOTP code of `offset` steps from now.
async fn sign_in(f: &Fixture, ip: &str, offset: i64) -> String {
    let pending = password_step(f, ip).await;
    let r = send(
        f,
        post(
            "/auth/totp",
            json!({ "code": code(f, offset) }),
            Some(&pending),
            ip,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.cookie().expect("session cookie")
}

#[tokio::test]
async fn api_needs_a_full_session() {
    let f = fixture(Policy::default());
    for path in [
        "/status",
        "/health",
        "/signal",
        "/unknown",
        "/auth/me",
        "/files/list",
    ] {
        let r = send(&f, get(path, None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    let r = send(&f, post("/status", json!({}), None, "192.0.2.1")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = send(&f, get("/health", Some(&"0".repeat(64)))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // The password alone opens only the TOTP step.
    let pending = password_step(&f, "192.0.2.1").await;
    for path in ["/status", "/health", "/signal", "/unknown"] {
        let r = send(&f, get(path, Some(&pending))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    let r = send(&f, get("/auth/me", Some(&pending))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.body["next"], "totp");

    // The client itself loads without a session, with the security headers.
    let r = send(
        &f,
        Request::get(format!("{BASE}/"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let csp = r.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        csp.contains("script-src 'self';") && csp.contains("wss://rcs.test:8444"),
        "{csp}"
    );
    assert_eq!(r.headers[header::X_FRAME_OPTIONS], "DENY");
    assert!(f.www.join("index.html").exists());
}

#[tokio::test]
async fn sign_in_and_out() {
    let f = fixture(Policy::default());
    let r = send(
        &f,
        post(
            "/auth/login",
            json!({ "password": PASSWORD }),
            None,
            "192.0.2.1",
        ),
    )
    .await;
    let set = r.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set.starts_with("__Secure-rcs_session="), "{set}");
    assert!(
        set.ends_with("; Path=/RemoteConnectionSystem/; Secure; HttpOnly; SameSite=Strict"),
        "{set}"
    );
    let pending = r.cookie().unwrap();
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": code(&f, 0) }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["authenticated"], true);
    let session = r.cookie().unwrap();
    assert_ne!(session, pending, "the token changes after TOTP");
    assert_eq!(
        send(&f, get("/health", Some(&pending))).await.status,
        StatusCode::UNAUTHORIZED
    );

    let me = send(&f, get("/auth/me", Some(&session))).await;
    assert_eq!(me.status, StatusCode::OK);
    assert!(me.body["expires_at"].as_str().unwrap().ends_with('Z'));
    let health = send(&f, get("/health", Some(&session))).await;
    assert_eq!(health.status, StatusCode::OK);
    // The agent is not running in tests; the request still gets past sign-in.
    assert_eq!(
        send(&f, get("/status", Some(&session))).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    let out = send(
        &f,
        post("/auth/logout", json!({}), Some(&session), "192.0.2.1"),
    )
    .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(
        out.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        send(&f, get("/health", Some(&session))).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&f, get("/auth/me", Some(&session))).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn wrong_password_and_wrong_code_look_the_same() {
    let f = fixture(Policy::default());
    let bad_password = send(
        &f,
        post(
            "/auth/login",
            json!({ "password": "a test password, long enougH" }),
            None,
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(bad_password.status, StatusCode::UNAUTHORIZED);
    assert!(bad_password.cookie().is_none());
    let pending = password_step(&f, "192.0.2.1").await;
    let wrong = format!(
        "{:06}",
        (code(&f, 0).parse::<u32>().unwrap() + 1) % 1_000_000
    );
    let bad_code = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": wrong }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(bad_code.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad_password.body, bad_code.body);
    assert_eq!(bad_code.body["error"], "invalid_credentials");
    // Malformed requests are refused without touching the session.
    let r = send(
        &f,
        post(
            "/auth/login",
            json!({ "pass": PASSWORD }),
            None,
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = send(
        &f,
        post(
            "/auth/login",
            json!({ "password": "x".repeat(2000) }),
            None,
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn totp_code_cannot_be_reused() {
    let f = fixture(Policy::default());
    let used = code(&f, 0);
    let session = sign_in(&f, "192.0.2.1", 0).await;
    send(
        &f,
        post("/auth/logout", json!({}), Some(&session), "192.0.2.1"),
    )
    .await;
    let pending = password_step(&f, "192.0.2.1").await;
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": used }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.body["error"], "invalid_credentials");
    // The previous step is older than the used one and is refused too.
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": code(&f, -1) }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": code(&f, 1) }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // The used step is on disk, so a restart does not reopen it.
    assert!(f.state.store.load().unwrap().totp_last_step.is_some());
}

#[tokio::test]
async fn recovery_code_works_once() {
    let f = fixture(Policy::default());
    let pending = password_step(&f, "192.0.2.1").await;
    let typed = f.codes[2].to_lowercase();
    let r = send(
        &f,
        post(
            "/auth/recovery",
            json!({ "code": typed }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(f.state.store.load().unwrap().recovery_sha256.len(), 7);
    let pending = password_step(&f, "192.0.2.1").await;
    let r = send(
        &f,
        post(
            "/auth/recovery",
            json!({ "code": f.codes[2] }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Without the password step a recovery code opens nothing.
    let r = send(
        &f,
        post(
            "/auth/recovery",
            json!({ "code": f.codes[3] }),
            None,
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(f.state.store.load().unwrap().recovery_sha256.len(), 7);
}

#[tokio::test]
async fn origin_and_request_header_are_checked() {
    let f = fixture(Policy::default());
    let body = || Body::from(json!({ "password": PASSWORD }).to_string());
    let login =
        || Request::post(format!("{BASE}/api/auth/login")).header("x-forwarded-for", "192.0.2.1");
    let cases = [
        login().header("x-rcs-request", "1").body(body()).unwrap(),
        login()
            .header(header::ORIGIN, "https://evil.test")
            .header("x-rcs-request", "1")
            .body(body())
            .unwrap(),
        login()
            .header(header::ORIGIN, "https://rcs.test:8444.evil.test")
            .header("x-rcs-request", "1")
            .body(body())
            .unwrap(),
        login().header(header::ORIGIN, ORIGIN).body(body()).unwrap(),
        login()
            .header(header::ORIGIN, ORIGIN)
            .header("x-rcs-request", "0")
            .body(body())
            .unwrap(),
    ];
    for req in cases {
        let r = send(&f, req).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
        assert!(r.cookie().is_none());
    }
    let session = sign_in(&f, "192.0.2.1", 0).await;
    let foreign = Request::get(format!("{BASE}/api/health"))
        .header(header::ORIGIN, "https://evil.test")
        .header(header::COOKIE, format!("{COOKIE}={session}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&f, foreign).await.status, StatusCode::FORBIDDEN);
    let logout_without_header = Request::post(format!("{BASE}/api/auth/logout"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, format!("{COOKIE}={session}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&f, logout_without_header).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&f, get("/health", Some(&session))).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn attempts_are_limited_per_address() {
    let f = fixture(Policy::default());
    let attacker = "203.0.113.7";
    for _ in 0..5 {
        let r = send(
            &f,
            post(
                "/auth/login",
                json!({ "password": "wrong password, long enough" }),
                None,
                attacker,
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Blocked now, even with the right password.
    let r = send(
        &f,
        post(
            "/auth/login",
            json!({ "password": PASSWORD }),
            None,
            attacker,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.body["error"], "too_many_attempts");
    let retry: u64 = r.headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((899..=900).contains(&retry), "{retry}");
    // Another address is not affected.
    password_step(&f, "192.0.2.1").await;

    // Wrong TOTP codes count too.
    let pending = password_step(&f, "198.51.100.1").await;
    for _ in 0..5 {
        let r = send(
            &f,
            post(
                "/auth/totp",
                json!({ "code": "000000" }),
                Some(&pending),
                "198.51.100.1",
            ),
        )
        .await;
        assert_ne!(r.status, StatusCode::OK);
    }
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": code(&f, 0) }),
            Some(&pending),
            "198.51.100.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn parallel_guesses_do_not_slip_past_the_limit() {
    let f = fixture(Policy::default());
    let pending = password_step(&f, "198.51.100.9").await;
    let guesses = (0..20).map(|i| {
        let req = post(
            "/auth/totp",
            json!({ "code": format!("{i:06}") }),
            Some(&pending),
            "198.51.100.9",
        );
        send(&f, req)
    });
    let replies = futures_util::future::join_all(guesses).await;
    let checked = replies
        .iter()
        .filter(|r| r.status == StatusCode::UNAUTHORIZED)
        .count();
    let blocked = replies
        .iter()
        .filter(|r| r.status == StatusCode::TOO_MANY_REQUESTS)
        .count();
    // A code among 000000..000019 could be right by chance; it would then be the one 200.
    assert!(checked <= 5, "{checked} codes were checked");
    assert_eq!(
        checked
            + blocked
            + replies
                .iter()
                .filter(|r| r.status == StatusCode::OK)
                .count(),
        20
    );
}

#[tokio::test]
async fn attempts_are_limited_in_total() {
    let f = fixture(Policy::default());
    for i in 0..20 {
        let ip = format!("203.0.113.{}", i / 4);
        let r = send(
            &f,
            post(
                "/auth/login",
                json!({ "password": "wrong password, long enough" }),
                None,
                &ip,
            ),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let r = send(
        &f,
        post(
            "/auth/login",
            json!({ "password": PASSWORD }),
            None,
            "192.0.2.99",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn forwarded_address_counts_only_from_loopback() {
    let f = fixture(Policy::default());
    // A direct connection from the LAN cannot dodge the limit by changing X-Forwarded-For.
    let router = app::router(f.state.clone(), &f.www, BASE).layer(MockConnectInfo(
        SocketAddr::from(([192, 168, 1, 50], 40000)),
    ));
    for i in 0..6 {
        let req = post(
            "/auth/login",
            json!({ "password": "wrong password, long enough" }),
            None,
            &format!("10.0.0.{i}"),
        );
        let status = router.clone().oneshot(req).await.unwrap().status();
        let expected = if i < 5 {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(status, expected, "attempt {i}");
    }
}

#[tokio::test]
async fn a_new_sign_in_ends_the_previous_session() {
    let f = fixture(Policy::default());
    let first = sign_in(&f, "192.0.2.1", 0).await;
    let pending = password_step(&f, "192.0.2.2").await;
    let r = send(
        &f,
        post(
            "/auth/recovery",
            json!({ "code": f.codes[0] }),
            Some(&pending),
            "192.0.2.2",
        ),
    )
    .await;
    let second = r.cookie().unwrap();
    assert_eq!(
        send(&f, get("/health", Some(&first))).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&f, get("/health", Some(&second))).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn sessions_expire() {
    let policy = Policy {
        pending_ttl: Duration::from_millis(300),
        idle_ttl: Duration::from_millis(600),
        ..Policy::default()
    };
    let f = fixture(policy);
    let pending = password_step(&f, "192.0.2.1").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let r = send(
        &f,
        post(
            "/auth/totp",
            json!({ "code": code(&f, 0) }),
            Some(&pending),
            "192.0.2.1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.body["error"], "unauthorized");

    let session = sign_in(&f, "192.0.2.1", 0).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        send(&f, get("/health", Some(&session))).await.status,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        send(&f, get("/health", Some(&session))).await.status,
        StatusCode::UNAUTHORIZED
    );
}

async fn serve(f: &Fixture) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app::router(f.state.clone(), &f.www, BASE);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    addr: SocketAddr,
    cookie: Option<&str>,
    origin: &str,
) -> Result<Socket, StatusCode> {
    let mut req = format!("ws://{addr}{BASE}/api/signal")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert(header::ORIGIN, origin.parse().unwrap());
    if let Some(c) = cookie {
        req.headers_mut()
            .insert(header::COOKIE, format!("{COOKIE}={c}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(r)) => Err(StatusCode::from_u16(r.status().as_u16()).unwrap()),
        Err(e) => panic!("{e}"),
    }
}

/// Reads until the server closes; returns the error codes it sent and the close code.
async fn until_closed(socket: &mut Socket, within: Duration) -> (Vec<String>, Option<u16>) {
    let mut errors = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => panic!("the socket stayed open; errors so far {errors:?}"),
            Ok(None | Some(Err(_))) => return (errors, None),
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "error" {
                    errors.push(v["code"].as_str().unwrap_or_default().to_string());
                }
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                return (errors, frame.map(|f| u16::from(f.code)));
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

#[tokio::test]
async fn websocket_needs_a_session_and_the_origin() {
    let f = fixture(Policy::default());
    let addr = serve(&f).await;
    assert_eq!(
        connect(addr, None, ORIGIN).await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );
    let pending = password_step(&f, "192.0.2.1").await;
    assert_eq!(
        connect(addr, Some(&pending), ORIGIN).await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );
    let session = sign_in(&f, "192.0.2.1", 0).await;
    assert_eq!(
        connect(addr, Some(&session), "https://evil.test")
            .await
            .err(),
        Some(StatusCode::FORBIDDEN)
    );

    let mut socket = connect(addr, Some(&session), ORIGIN).await.unwrap();
    socket
        .send(Message::Text(
            json!({ "v": 1, "type": "ping", "id": 5 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let pong: Value = match socket.next().await.unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(pong["type"], "pong");

    // Logout closes the socket at once.
    let out = send(
        &f,
        post("/auth/logout", json!({}), Some(&session), "192.0.2.1"),
    )
    .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let (errors, close) = until_closed(&mut socket, Duration::from_secs(2)).await;
    assert_eq!(errors, ["session_ended"]);
    assert_eq!(close, Some(crate::signal::CLOSE_SESSION_ENDED));
}

#[tokio::test]
async fn quiet_websocket_is_pinged_and_a_silent_one_is_dropped() {
    let policy = Policy {
        socket_ping: Duration::from_millis(500),
        socket_silence: Duration::from_millis(2500),
        ..Policy::default()
    };
    let f = fixture(policy);
    let addr = serve(&f).await;
    let session = sign_in(&f, "192.0.2.1", 0).await;

    // A browser answers pings by itself (tungstenite does the same while it reads): the socket stays open.
    let mut socket = connect(addr, Some(&session), ORIGIN).await.unwrap();
    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while let Ok(frame) = tokio::time::timeout_at(deadline, socket.next()).await {
        match frame {
            Some(Ok(Message::Ping(_))) => pings += 1,
            other => panic!("the socket ended: {other:?}"),
        }
    }
    assert!(pings >= 2, "{pings} pings");

    // A peer that has gone away answers nothing: the server drops the socket.
    let mut silent = connect(addr, Some(&session), ORIGIN).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let (errors, close) = until_closed(&mut silent, Duration::from_secs(2)).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(close, None);
}

#[tokio::test]
async fn websocket_closes_when_the_session_runs_out_or_is_replaced() {
    let policy = Policy {
        absolute_ttl: Duration::from_millis(1500),
        ..Policy::default()
    };
    let f = fixture(policy);
    let addr = serve(&f).await;
    let session = sign_in(&f, "192.0.2.1", 0).await;
    let mut socket = connect(addr, Some(&session), ORIGIN).await.unwrap();
    let (errors, close) = until_closed(&mut socket, Duration::from_secs(4)).await;
    assert_eq!(errors, ["session_ended"]);
    assert_eq!(close, Some(crate::signal::CLOSE_SESSION_ENDED));

    let f = fixture(Policy::default());
    let addr = serve(&f).await;
    let session = sign_in(&f, "192.0.2.1", 0).await;
    let mut socket = connect(addr, Some(&session), ORIGIN).await.unwrap();
    let pending = password_step(&f, "192.0.2.2").await;
    let r = send(
        &f,
        post(
            "/auth/recovery",
            json!({ "code": f.codes[1] }),
            Some(&pending),
            "192.0.2.2",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let (errors, _) = until_closed(&mut socket, Duration::from_secs(2)).await;
    assert_eq!(errors, ["session_ended"]);
}

mod files;
