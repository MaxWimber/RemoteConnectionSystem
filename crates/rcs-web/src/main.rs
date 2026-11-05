//! `rcs-web`: the web client, sign-in, signaling and, from phase 7, files.
//!
//! Listens on loopback only; Caddy publishes it (docs/SECURITY.md). Without a password and TOTP set up locally
//! with `rcs-web set-password` and `rcs-web enroll-totp` it does not start.

mod agent;
mod app;
mod audit;
mod auth;
mod console;
mod secrets;
mod signal;
mod totp;
mod util;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use rcs_protocol::BUILD_ID;
use rcs_protocol::agent::PIPE_NAME;
use serde_json::json;
use tracing::info;

use crate::app::{AppState, Settings};
use crate::audit::Audit;
use crate::auth::Policy;
use crate::secrets::{MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS, RECOVERY_CODES, Store};

const USAGE: &str = "\
Usage:
  rcs-web [--listen ADDR] [--static DIR] [--pipe NAME] [--base PATH] [--origin URL] [--secrets DIR] [--audit FILE]
  rcs-web set-password [--secrets DIR] [--audit FILE]
  rcs-web enroll-totp [--secrets DIR] [--audit FILE]

  --listen   loopback address, default 127.0.0.1:8080
  --static   the built web client, default web/dist
  --base     URL prefix, default /RemoteConnectionSystem
  --origin   the public origin browsers use, default https://maxwimber.ru:8444
  --secrets  default %ProgramData%\\RCS\\secrets
  --audit    default %ProgramData%\\RCS\\logs\\audit.log";

struct Options {
    listen: SocketAddr,
    static_dir: PathBuf,
    pipe: String,
    base: String,
    origin: String,
    secrets: PathBuf,
    audit: PathBuf,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
        static_dir: PathBuf::from("web").join("dist"),
        pipe: PIPE_NAME.into(),
        base: "/RemoteConnectionSystem".into(),
        origin: "https://maxwimber.ru:8444".into(),
        secrets: secrets::default_dir(),
        audit: audit::default_path(),
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => options.listen = value.parse().map_err(|e| format!("{flag}: {e}"))?,
            "--static" => options.static_dir = PathBuf::from(value),
            "--pipe" => options.pipe = value.clone(),
            "--base" => options.base = value.trim_end_matches('/').to_string(),
            "--origin" => options.origin = value.trim_end_matches('/').to_string(),
            "--secrets" => options.secrets = PathBuf::from(value),
            "--audit" => options.audit = PathBuf::from(value),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if !options.base.starts_with('/') {
        return Err("--base must start with /".into());
    }
    if !(options.origin.starts_with("https://") || options.origin.starts_with("http://"))
        || !options.origin.is_ascii()
    {
        return Err("--origin must look like https://host:port".into());
    }
    // Only Caddy may publish rcs-web (docs/SECURITY.md); a LAN or public address is refused.
    if !options.listen.ip().is_loopback() {
        return Err(
            "--listen must be a loopback address: rcs-web is published through Caddy only".into(),
        );
    }
    Ok(options)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.first().map(String::as_str) {
        Some(c @ ("set-password" | "enroll-totp")) => (c, &args[1..]),
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => ("serve", &args[..]),
    };
    let options = match parse(rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command {
        "set-password" => set_password(&options),
        "enroll-totp" => enroll_totp(&options),
        _ => {
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .init();
            serve(options).await
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Refuses to start until both secrets are set and readable.
fn check_secrets(store: &Store) -> Result<(), String> {
    store.prepare()?;
    let file = store.load()?;
    let hash = file
        .password_hash
        .as_deref()
        .ok_or("пароль не задан: выполните rcs-web set-password")?;
    secrets::check_hash(hash)?;
    if file.totp_dpapi.is_none() {
        return Err("TOTP не настроен: выполните rcs-web enroll-totp".into());
    }
    let secret = file.totp_secret()?;
    if secret.len() < totp::SECRET_BYTES {
        return Err("секрет TOTP слишком короткий: выполните rcs-web enroll-totp".into());
    }
    Ok(())
}

async fn serve(options: Options) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    check_secrets(&store)?;
    let audit = Audit::open(options.audit.clone())?;
    audit.event("started", None, json!({ "build": BUILD_ID }));
    let state = Arc::new(AppState::new(Settings {
        pipe: options.pipe.clone(),
        base: options.base.clone(),
        origin: options.origin.clone(),
        policy: Policy::default(),
        store,
        audit,
    }));
    app::spawn_sweeper(state.clone());
    let router = app::router(state, &options.static_dir, &options.base);
    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .map_err(|e| format!("listen {}: {e}", options.listen))?;
    info!(listen = %options.listen, base = %options.base, origin = %options.origin, "rcs-web started");
    println!(
        "rcs-web {BUILD_ID}: http://{}{}/ for {}",
        options.listen, options.base, options.origin
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| format!("server: {e}"))
}

fn audit_best_effort(options: &Options, event: &str) {
    if let Ok(audit) = Audit::open(options.audit.clone()) {
        audit.event(event, None, json!({}));
        // The writer thread flushes before the process exits only if given a moment.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn set_password(options: &Options) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    store.prepare()?;
    println!(
        "Пароль для входа в RemoteConnectionSystem: не меньше {MIN_PASSWORD_CHARS} символов. Ввод не отображается."
    );
    let first = console::read_line("Пароль: ", true).map_err(|e| e.to_string())?;
    let chars = first.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(format!(
            "пароль короче {MIN_PASSWORD_CHARS} символов ({chars})"
        ));
    }
    if first.len() > MAX_PASSWORD_BYTES {
        return Err(format!("пароль длиннее {MAX_PASSWORD_BYTES} байт"));
    }
    let second = console::read_line("Ещё раз: ", true).map_err(|e| e.to_string())?;
    if first != second {
        return Err("пароли не совпали".into());
    }
    let hash = secrets::hash_password(&first)?;
    let mut file = store.load()?;
    file.password_hash = Some(hash);
    store.save(&file)?;
    audit_best_effort(options, "password_changed");
    println!("Пароль сохранён в {}.", store.dir().display());
    println!("Он действует сразу. Уже открытая сессия остаётся до выхода или истечения.");
    if file.totp_dpapi.is_none() {
        println!("Следующий шаг: rcs-web enroll-totp");
    }
    Ok(())
}

fn print_qr(text: &str) -> Result<(), String> {
    use qrcode::QrCode;
    use qrcode::render::unicode::Dense1x2;
    let code = QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let picture = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{picture}");
    Ok(())
}

fn enroll_totp(options: &Options) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    store.prepare()?;
    let secret = util::random::<{ totp::SECRET_BYTES }>();
    let account = options
        .origin
        .split("://")
        .nth(1)
        .and_then(|h| h.split(':').next())
        .unwrap_or("rcs");
    let uri = totp::otpauth_uri(&secret, account);
    println!(
        "Отсканируйте QR-код приложением для одноразовых кодов (Google Authenticator, Aegis и другие)."
    );
    print_qr(&uri)?;
    println!(
        "Если QR не читается, введите ключ вручную: {}",
        totp::base32(&secret)
    );
    println!("Тип: по времени, 6 цифр, 30 секунд, SHA-1.");
    let mut matched = None;
    for _ in 0..3 {
        let code =
            console::read_line("Текущий код из приложения: ", false).map_err(|e| e.to_string())?;
        let now = totp::step_of(util::unix_seconds(SystemTime::now()));
        matched = totp::verify(&secret, &code, now, None);
        if matched.is_some() {
            break;
        }
        println!("Код не подошёл. Проверьте время на телефоне и попробуйте снова.");
    }
    let step = matched.ok_or("код не подтверждён, секрет не сохранён")?;
    let codes: Vec<String> = (0..RECOVERY_CODES)
        .map(|_| secrets::new_recovery_code())
        .collect();
    let mut file = store.load()?;
    file.set_totp_secret(&secret)?;
    file.totp_last_step = Some(step);
    file.set_recovery_codes(&codes);
    store.save(&file)?;
    audit_best_effort(options, "totp_enrolled");
    println!();
    println!("TOTP сохранён. Коды восстановления, каждый действует один раз:");
    for code in &codes {
        println!("  {code}");
    }
    println!("Запишите их и храните отдельно от телефона. Больше они не показываются.");
    println!("Прежний секрет TOTP и прежние коды восстановления больше не действуют.");
    if file.password_hash.is_none() {
        println!("Следующий шаг: rcs-web set-password");
    }
    Ok(())
}
